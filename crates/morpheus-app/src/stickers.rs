//! Native sticker picker and the one tracked sticker send.
//!
//! The Matrix core owns each catalog, its media addresses and every
//! authorization check. This controller keeps the opaque catalog, publishes
//! sanitized labels with opaque keys to Slint, and binds every asynchronous
//! result to its picker session before publication. A picker belongs to one
//! account selection, room and optional thread. Every command is admitted only
//! while the picker still matches the visible destination, its composer and
//! its selection; the existing selection revocation (account switch,
//! protected switch, removal or detach) and any change of the visible
//! destination close it, so returning later never revives a choice, preview
//! or consent.
//!
//! Sticker images are public pack media. In an encrypted conversation they
//! are fetched or sent only after an explicit consent for the exact catalog;
//! the labels stay usable without it. A rebuilt catalog needs consent again.
//!
//! Previews run through one latest-only lane per picker: a newer request
//! replaces a waiting one and cancels a running fetch, so at most one fetch
//! and one codec run exist at a time, and nothing superseded is decoded.
//!
//! Sends go through the account's persistent SDK queue, one tracked sticker at
//! a time. A send whose outcome is not known is held: nothing new is sent to
//! its destination and nothing is retried or replayed. When the enqueue
//! itself did not return there is no transaction to look for, so only the
//! user's confirmation that they checked the conversation lifts the hold.
//! When an observed request loses its outcome (its update stream ended or
//! lagged, or its selection ended), the hold keeps the request's transaction:
//! finding exactly that transaction still queued observes it again in place,
//! and otherwise the user confirms. Any other queued sticker may be observed
//! meanwhile, but it never lifts a hold. Holds live in memory for this run and
//! belong to the account and destination, not to one selection, so switching
//! away and back keeps them. Nothing here ever sends a request again.

use crate::{
    AppWindow, StickerItem,
    image_previews::{self, PreviewError},
    live::LiveUi,
};
use matrix_sdk::sync::RoomUpdates;
use morpheus_matrix::{
    AccountSelection, MatrixCoreError, PendingSticker, PublicMediaConsent, QueuedMessage,
    QueuedSendUpdate, StickerCatalog, StickerEntry, StickerPack,
};
use slint::{ComponentHandle, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::{
    runtime::Handle,
    sync::{Notify, broadcast, mpsc, oneshot, watch},
};
use zeroize::Zeroizing;

/// Bounds and pauses of the picker's background work. Tests shorten them.
#[derive(Clone, Copy)]
struct Timing {
    catalog: Duration,
    preview: Duration,
    preview_delay: Duration,
    worker_wait: Duration,
    watch_interval: Duration,
    watch_minimum: Duration,
    sent_status: Duration,
    recheck: Duration,
    /// Queue checks for an uncertain send before only the user can resolve it.
    checks: u32,
}

const LIVE_TIMING: Timing = Timing {
    catalog: Duration::from_secs(20),
    preview: Duration::from_secs(45),
    preview_delay: Duration::from_millis(200),
    worker_wait: Duration::from_secs(5),
    watch_interval: Duration::from_secs(3),
    watch_minimum: Duration::from_secs(1),
    sent_status: Duration::from_secs(5),
    recheck: Duration::from_secs(3),
    checks: 10,
};
const MAX_QUERY_CHARS: usize = 100;
const MAX_ROWS: usize = 300;

/// Unresolved sends held at once. While this many are held no new sticker is
/// sent anywhere; a hold is never dropped to make room.
const MAX_HOLDS: usize = 16;

const UNCERTAIN_STATUS: &str = "This sticker may or may not have been queued. Check the \
    conversation, then confirm; no other sticker is sent here until you do.";
const CHECKING_STATUS: &str = "This sticker's progress was lost. Looking for it in the queue; no \
    other sticker is sent here until it is found or you confirm you checked the conversation.";
const LOST_STATUS: &str = "This sticker's progress was lost. Open the sticker picker here to look \
    for it again, or check the conversation and confirm.";
const GONE_STATUS: &str = "This sticker is no longer queued: it was sent or cancelled. Check the \
    conversation, then confirm to send stickers here again.";
const ALREADY_SENT: &str = "The sticker was already sent.";
const CANCEL_FAILED: &str = "The sticker could not be cancelled yet.";
const RETRY_FAILED: &str = "The sticker could not be retried. Cancel it or check the conversation.";
const BUSY_NOTICE: &str = "Wait for the previous sticker to finish, or resolve it.";
const HELD_NOTICE: &str = "An earlier sticker here is unresolved. Check the conversation and \
    confirm first.";
const FULL_NOTICE: &str = "Too many sticker sends are unresolved. Check their conversations and \
    confirm them first.";

/// The account selection a picker or a send belongs to.
pub(crate) trait Selection: Clone + Send + Sync + 'static {
    fn account_id(&self) -> &str;

    fn is_current(&self) -> bool;

    fn revoked(&self) -> impl Future<Output = ()> + Send;
}

/// An opaque catalog as the controller sees it: sanitized labels and
/// indexes, and a consent token bound to it.
pub(crate) trait Catalog: Send + Sync + 'static {
    type Consent: Copy + Send + Sync + 'static;

    fn entries(&self) -> &[StickerEntry];

    fn packs(&self) -> &[StickerPack];

    fn skipped(&self) -> usize;

    fn unavailable_packs(&self) -> usize;

    fn requires_public_media_consent(&self) -> bool;

    fn public_media_consent(&self) -> Self::Consent;
}

/// One request in the persistent queue, observed by its transaction.
pub(crate) trait QueuedSticker: Send + 'static {
    fn transaction_id(&self) -> &str;

    fn next_update(&mut self) -> impl Future<Output = QueuedSendUpdate> + Send;

    fn abort(&self) -> impl Future<Output = Result<bool, MatrixCoreError>> + Send;

    fn retry_wedged(&self) -> impl Future<Output = Result<(), MatrixCoreError>> + Send;
}

/// A sticker the queue still holds: the newest one for a destination, or
/// the one with an exact transaction.
pub(crate) struct Pending<Q> {
    label: String,
    wedged: bool,
    others: usize,
    /// More stickers were queued than were read; `others` is a floor.
    truncated: bool,
    queued: Q,
}

type ConsentOf<B> = <<B as StickerBackend>::Catalog as Catalog>::Consent;

/// The Matrix core as the picker uses it: the live account manager in the
/// app, a recording fake in tests.
pub(crate) trait StickerBackend: Send + Sync + 'static {
    type Selection: Selection;
    type Catalog: Catalog;
    type Queued: QueuedSticker;

    fn selection(&self, account: &str) -> Option<Self::Selection>;

    fn catalog(
        &self,
        selection: &Self::Selection,
        room: &str,
        thread: Option<&str>,
    ) -> impl Future<Output = Result<Self::Catalog, MatrixCoreError>> + Send;

    fn refresh(
        &self,
        catalog: &Self::Catalog,
    ) -> impl Future<Output = Result<Option<Self::Catalog>, MatrixCoreError>> + Send;

    fn preview(
        &self,
        catalog: &Self::Catalog,
        entry: usize,
        consent: Option<ConsentOf<Self>>,
    ) -> impl Future<Output = Result<Vec<u8>, MatrixCoreError>> + Send;

    fn send(
        &self,
        catalog: &Self::Catalog,
        entry: usize,
        consent: Option<ConsentOf<Self>>,
    ) -> impl Future<Output = Result<Self::Queued, MatrixCoreError>> + Send;

    fn pending(
        &self,
        selection: &Self::Selection,
        room: &str,
        thread: Option<&str>,
    ) -> impl Future<Output = Result<Option<Pending<Self::Queued>>, MatrixCoreError>> + Send;

    /// Exactly the request with `transaction`, if the room's queue still
    /// holds it. Read-only.
    fn queued(
        &self,
        selection: &Self::Selection,
        room: &str,
        transaction: &str,
    ) -> impl Future<Output = Result<Option<Pending<Self::Queued>>, MatrixCoreError>> + Send;

    fn room_updates(
        &self,
        account: &str,
    ) -> impl Future<Output = Option<broadcast::Receiver<RoomUpdates>>> + Send;
}

impl Selection for AccountSelection {
    fn account_id(&self) -> &str {
        AccountSelection::account_id(self)
    }

    fn is_current(&self) -> bool {
        AccountSelection::is_current(self)
    }

    fn revoked(&self) -> impl Future<Output = ()> + Send {
        AccountSelection::revoked(self)
    }
}

impl Catalog for StickerCatalog {
    type Consent = PublicMediaConsent;

    fn entries(&self) -> &[StickerEntry] {
        StickerCatalog::entries(self)
    }

    fn packs(&self) -> &[StickerPack] {
        StickerCatalog::packs(self)
    }

    fn skipped(&self) -> usize {
        StickerCatalog::skipped(self)
    }

    fn unavailable_packs(&self) -> usize {
        StickerCatalog::unavailable_packs(self)
    }

    fn requires_public_media_consent(&self) -> bool {
        StickerCatalog::requires_public_media_consent(self)
    }

    fn public_media_consent(&self) -> PublicMediaConsent {
        StickerCatalog::public_media_consent(self)
    }
}

impl QueuedSticker for QueuedMessage {
    fn transaction_id(&self) -> &str {
        QueuedMessage::transaction_id(self)
    }

    /// A lagged subscription may have skipped the outcome; it reports a lost
    /// outcome instead of waiting for an update that may never come.
    fn next_update(&mut self) -> impl Future<Output = QueuedSendUpdate> + Send {
        QueuedMessage::next_update_or_loss(self)
    }

    fn abort(&self) -> impl Future<Output = Result<bool, MatrixCoreError>> + Send {
        QueuedMessage::abort(self)
    }

    fn retry_wedged(&self) -> impl Future<Output = Result<(), MatrixCoreError>> + Send {
        QueuedMessage::retry_wedged(self)
    }
}

/// The live Matrix core behind the picker.
pub(crate) struct LiveStickers(Arc<LiveUi>);

impl StickerBackend for LiveStickers {
    type Selection = AccountSelection;
    type Catalog = StickerCatalog;
    type Queued = QueuedMessage;

    fn selection(&self, account: &str) -> Option<AccountSelection> {
        self.0.manager.account_selection(account)
    }

    async fn catalog(
        &self,
        selection: &AccountSelection,
        room: &str,
        thread: Option<&str>,
    ) -> Result<StickerCatalog, MatrixCoreError> {
        let manager = &self.0.manager;
        manager.sticker_catalog_for(selection, room, thread).await
    }

    async fn refresh(
        &self,
        catalog: &StickerCatalog,
    ) -> Result<Option<StickerCatalog>, MatrixCoreError> {
        self.0.manager.refresh_sticker_catalog(catalog).await
    }

    async fn preview(
        &self,
        catalog: &StickerCatalog,
        entry: usize,
        consent: Option<PublicMediaConsent>,
    ) -> Result<Vec<u8>, MatrixCoreError> {
        let manager = &self.0.manager;
        let consent = consent.as_ref();
        let preview = manager
            .download_sticker_preview_for(catalog, entry, consent)
            .await?;
        Ok(preview.into_bytes())
    }

    async fn send(
        &self,
        catalog: &StickerCatalog,
        entry: usize,
        consent: Option<PublicMediaConsent>,
    ) -> Result<QueuedMessage, MatrixCoreError> {
        let consent = consent.as_ref();
        self.0.send_sticker(catalog, entry, consent).await
    }

    async fn pending(
        &self,
        selection: &AccountSelection,
        room: &str,
        thread: Option<&str>,
    ) -> Result<Option<Pending<QueuedMessage>>, MatrixCoreError> {
        let manager = &self.0.manager;
        let lookup = manager.pending_sticker_for(selection, room, thread);
        Ok(lookup.await?.map(pending))
    }

    async fn queued(
        &self,
        selection: &AccountSelection,
        room: &str,
        transaction: &str,
    ) -> Result<Option<Pending<QueuedMessage>>, MatrixCoreError> {
        let manager = &self.0.manager;
        let lookup = manager.queued_sticker_for(selection, room, transaction);
        Ok(lookup.await?.map(pending))
    }

    async fn room_updates(&self, account: &str) -> Option<broadcast::Receiver<RoomUpdates>> {
        let (owner, updates) = self.0.manager.subscribe_room_updates().await.ok()?;
        (owner == account).then_some(updates)
    }
}

fn pending(found: PendingSticker) -> Pending<QueuedMessage> {
    let PendingSticker {
        label,
        wedged,
        others,
        truncated,
        queued,
    } = found;
    Pending {
        label,
        wedged,
        others,
        truncated,
        queued,
    }
}

/// Install the picker on the window. The callbacks own the controller, and
/// picker state ends with its account selection.
pub(crate) fn register(
    ui: &AppWindow,
    live: Arc<LiveUi>,
    runtime: Handle,
) -> Arc<StickerUi<LiveStickers>> {
    StickerUi::install(ui, Arc::new(LiveStickers(live)), runtime, LIVE_TIMING)
}

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

/// The single sticker decode slot. A superseded request cannot recall a
/// codec call already running; the permit stays taken until it returns.
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

type PreviewResult = Result<SharedPixelBuffer<Rgba8Pixel>, &'static str>;
type Jobs<J> = watch::Receiver<Option<Arc<J>>>;

/// The effects of one preview: the controller in the app, counters in tests.
trait PreviewWork: Send + Sync + 'static {
    type Job: Send + Sync + 'static;

    /// The job still belongs to the open picker, its request and selection.
    fn current(&self, job: &Self::Job) -> bool;

    fn fetch(
        &self,
        job: &Self::Job,
    ) -> impl Future<Output = Result<Zeroizing<Vec<u8>>, &'static str>> + Send;

    /// Start the codec for a still-wanted job. The permit travels with the
    /// run and is released only when the codec returns.
    fn decode(
        &self,
        permit: Permit,
        bytes: Zeroizing<Vec<u8>>,
    ) -> Result<oneshot::Receiver<PreviewResult>, &'static str>;

    fn finish(&self, job: &Self::Job, result: PreviewResult);
}

fn still_wanted<J>(jobs: &Jobs<J>, job: &J) -> bool {
    jobs.borrow()
        .as_ref()
        .is_some_and(|latest| std::ptr::eq(Arc::as_ptr(latest), job))
}

/// The latest-only preview lane of one picker. It runs one job at a time: a
/// newer job replaces a waiting one and cancels a running fetch, and a
/// superseded or closed job never reaches the codec. It ends with the picker
/// (its job sender is dropped) or when stopped.
async fn pump_previews<W: PreviewWork>(
    work: Arc<W>,
    mut jobs: Jobs<W::Job>,
    stop: Arc<Stop>,
    slot: Arc<AtomicBool>,
    timing: Timing,
) {
    loop {
        tokio::select! {
            biased;
            _ = stop.wait() => return,
            changed = jobs.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        // Rapid replacements collapse: only the job still wanted after a
        // quiet pause is started.
        let job = loop {
            let job = jobs.borrow_and_update().clone();
            tokio::select! {
                biased;
                _ = stop.wait() => return,
                changed = jobs.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                _ = tokio::time::sleep(timing.preview_delay) => break job,
            }
        };
        let Some(job) = job else {
            continue;
        };
        let result = run_preview(&*work, &job, &jobs, &stop, &slot, timing.worker_wait).await;
        if let Some(result) = result {
            work.finish(&job, result);
        }
    }
}

/// Fetch and decode one job. `None` when it was superseded, its context
/// closed or the lane stopped: nothing obsolete is decoded or published.
async fn run_preview<W: PreviewWork>(
    work: &W,
    job: &Arc<W::Job>,
    jobs: &Jobs<W::Job>,
    stop: &Stop,
    slot: &Arc<AtomicBool>,
    worker_wait: Duration,
) -> Option<PreviewResult> {
    if !work.current(job) {
        return None;
    }
    let mut newer = jobs.clone();
    let fetched = tokio::select! {
        biased;
        _ = stop.wait() => return None,
        _ = newer.changed() => return None,
        fetched = work.fetch(job) => fetched,
    };
    let bytes = match fetched {
        Ok(bytes) => bytes,
        Err(text) => return Some(Err(text)),
    };
    let deadline = tokio::time::Instant::now() + worker_wait;
    let permit = loop {
        // A superseded or closed job never starts decoding, even when the
        // decode slot is free.
        if stop.is_stopped() || !still_wanted(jobs, job) || !work.current(job) {
            return None;
        }
        if let Some(permit) = Permit::acquire(slot) {
            break permit;
        }
        if tokio::time::Instant::now() >= deadline {
            return Some(Err(preview_failure(PreviewError::Busy)));
        }
        tokio::select! {
            biased;
            _ = stop.wait() => return None,
            _ = newer.changed() => return None,
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    };
    let decoded = match work.decode(permit, bytes) {
        Ok(decoded) => decoded,
        Err(text) => return Some(Err(text)),
    };
    tokio::select! {
        biased;
        _ = stop.wait() => None,
        _ = newer.changed() => None,
        decoded = decoded => Some(decoded.unwrap_or(Err(preview_failure(PreviewError::Invalid)))),
    }
}

/// Decode on a dedicated thread with the shared preview limits.
fn decode_on_worker(
    permit: Permit,
    bytes: Zeroizing<Vec<u8>>,
) -> Result<oneshot::Receiver<PreviewResult>, &'static str> {
    let (sender, receiver) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("sticker-preview".into())
        .spawn(move || {
            // The decode slot stays taken until the codec actually returns.
            let _permit = permit;
            let decoded = image_previews::decode_rgba(bytes)
                .map(|(width, height, rgba)| pixels(&rgba, width, height))
                .map_err(preview_failure);
            let _ = sender.send(decoded);
        });
    match spawned {
        Ok(_) => Ok(receiver),
        Err(_) => Err(preview_failure(PreviewError::Unavailable)),
    }
}

fn pixels(rgba: &[u8], width: u32, height: u32) -> SharedPixelBuffer<Rgba8Pixel> {
    SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba, width, height)
}

struct PreviewJob<B: StickerBackend> {
    session: u64,
    request: u64,
    catalog: Arc<B::Catalog>,
    entry: usize,
    consent: Option<ConsentOf<B>>,
}

struct PreviewOwner<B: StickerBackend> {
    owner: Arc<StickerUi<B>>,
    weak: slint::Weak<AppWindow>,
}

impl<B: StickerBackend> PreviewWork for PreviewOwner<B> {
    type Job = PreviewJob<B>;

    fn current(&self, job: &PreviewJob<B>) -> bool {
        self.owner.preview_current(job.session, job.request)
    }

    async fn fetch(&self, job: &PreviewJob<B>) -> Result<Zeroizing<Vec<u8>>, &'static str> {
        let backend = &self.owner.backend;
        let request = backend.preview(&job.catalog, job.entry, job.consent);
        match tokio::time::timeout(self.owner.timing.preview, request).await {
            Ok(Ok(bytes)) => Ok(Zeroizing::new(bytes)),
            Ok(Err(error)) => {
                if matches!(error, MatrixCoreError::StickerConsentRequired) {
                    self.owner.require_consent(job.session);
                }
                Err(download_failure(&error))
            }
            Err(_) => Err("The preview timed out."),
        }
    }

    fn decode(
        &self,
        permit: Permit,
        bytes: Zeroizing<Vec<u8>>,
    ) -> Result<oneshot::Receiver<PreviewResult>, &'static str> {
        decode_on_worker(permit, bytes)
    }

    fn finish(&self, job: &PreviewJob<B>, result: PreviewResult) {
        self.owner.finish_preview(job.session, job.request, result);
        self.owner.schedule_publish(&self.weak);
    }
}

enum Phase<C> {
    Loading,
    Ready(Arc<C>),
    Failed(String),
}

#[derive(Default)]
struct PreviewSlot {
    request: u64,
    entry: Option<usize>,
    pixels: Option<SharedPixelBuffer<Rgba8Pixel>>,
    status: String,
}

struct Picker<B: StickerBackend> {
    session: u64,
    destination: Destination,
    selection: B::Selection,
    phase: Phase<B::Catalog>,
    query: String,
    pack: Option<usize>,
    rows: Vec<usize>,
    highlighted: Option<usize>,
    /// The destination needs consent: from the catalog, or because a later
    /// action found the room encrypted.
    consent_required: bool,
    consent: Option<ConsentOf<B>>,
    notice: String,
    preview: PreviewSlot,
    /// The newest wanted preview; the picker's lane runs only this one.
    jobs: watch::Sender<Option<Arc<PreviewJob<B>>>>,
    stop: Arc<Stop>,
}

impl<B: StickerBackend> Picker<B> {
    fn catalog(&self) -> Option<&Arc<B::Catalog>> {
        match &self.phase {
            Phase::Ready(catalog) => Some(catalog),
            _ => None,
        }
    }

    fn current_entry(&self) -> Option<usize> {
        let row = self.highlighted?;
        self.rows.get(row).copied()
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

    /// Forget the preview and withdraw any wanted job from the lane.
    fn clear_preview(&mut self) {
        self.preview = PreviewSlot::default();
        self.jobs.send_replace(None);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Cancel,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SendPhase {
    /// Queued and observed by its transaction.
    Observed,
    /// Sent or cancelled.
    Finished,
    /// The outcome was lost. The request is held with its transaction and
    /// never reported as finished.
    Lost,
}

/// Why a send is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    /// The enqueue did not return, so there is no transaction to look for.
    Enqueue,
    /// The outcome was lost; the transaction can be looked up again.
    Lost,
    /// The exact transaction is being looked up.
    Checking,
    /// The exact transaction is no longer queued: it was sent or cancelled.
    Gone,
}

/// A send whose outcome is not known. Nothing new is sent to its destination
/// until exactly this request is found still queued or the user confirms they
/// checked the conversation. It belongs to the account and destination, not
/// to one selection, and lives in memory for this run.
struct Held {
    id: u64,
    destination: Destination,
    /// The request's transaction, whenever it was known.
    transaction: Option<String>,
    hold: Hold,
}

struct Tracked<B: StickerBackend> {
    send: u64,
    destination: Destination,
    selection: B::Selection,
    transaction: String,
    status: String,
    phase: SendPhase,
    can_cancel: bool,
    can_retry: bool,
    /// One command waits at most; the controls stay disabled while it runs.
    commands: Option<mpsc::Sender<Command>>,
}

struct State<B: StickerBackend> {
    picker: Option<Picker<B>>,
    tracked: Option<Tracked<B>>,
    enqueuing: bool,
    /// Unresolved sends, oldest first. New sends stop at `MAX_HOLDS`, so no
    /// more than that are ever held.
    held: Vec<Held>,
    /// The hold and the tracked send the status last showed. Confirmation
    /// and commands apply only to what was shown.
    shown_hold: Option<u64>,
    shown_send: Option<u64>,
}

impl<B: StickerBackend> Default for State<B> {
    fn default() -> Self {
        Self {
            picker: None,
            tracked: None,
            enqueuing: false,
            held: Vec::new(),
            shown_hold: None,
            shown_send: None,
        }
    }
}

/// What one admitted send carries into its task.
struct Prepared<B: StickerBackend> {
    session: u64,
    catalog: Arc<B::Catalog>,
    entry: usize,
    consent: Option<ConsentOf<B>>,
    selection: B::Selection,
    destination: Destination,
    label: String,
}

enum Event {
    Update(QueuedSendUpdate),
    Command(Option<Command>),
}

pub(crate) struct StickerUi<B: StickerBackend> {
    backend: Arc<B>,
    runtime: Handle,
    timing: Timing,
    state: Mutex<State<B>>,
    sessions: AtomicU64,
    previews: AtomicU64,
    sends: AtomicU64,
    holds: AtomicU64,
    worker: Arc<AtomicBool>,
}

fn next_id(counter: &AtomicU64) -> u64 {
    counter.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
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

/// The composer the picker belongs to is visible and idle, and no modal is
/// open. A room picker is for new messages only; reply and edit modes close it.
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

fn picker_valid<B: StickerBackend>(ui: &AppWindow, picker: &Picker<B>) -> bool {
    let threaded = picker.destination.threaded();
    binding_matches(
        picker.selection.is_current(),
        allowed(ui, threaded),
        destination(ui, threaded).as_ref(),
        &picker.destination,
    )
}

/// The place is the visible conversation, with its thread pane shown.
fn shown_at(ui: &AppWindow, place: &Destination) -> bool {
    let threaded = place.threaded();
    ui.get_live_session()
        && destination(ui, threaded).as_ref() == Some(place)
        && (!threaded || ui.get_side_panel_tab() == 1)
}

fn tracked_visible<B: StickerBackend>(ui: &AppWindow, tracked: &Tracked<B>) -> bool {
    tracked.selection.is_current() && shown_at(ui, &tracked.destination)
}

/// One sticker at a time: a request still observed under the current
/// selection blocks the next one.
fn blocks_send<B: StickerBackend>(tracked: &Tracked<B>) -> bool {
    tracked.phase == SendPhase::Observed && tracked.selection.is_current()
}

/// Whether a request found in the queue may be observed now: it never
/// displaces an observed request and is never observed twice. Observing a
/// request lifts no hold by itself; see `exact_hold`.
fn may_adopt<B: StickerBackend>(tracked: Option<&Tracked<B>>, transaction: &str) -> bool {
    tracked.is_none_or(|tracked| {
        tracked.phase != SendPhase::Observed && tracked.transaction != transaction
    })
}

/// The hold that observing exactly `transaction` at `place` lifts, if any.
/// Any other request, however recent, never resolves a hold.
fn exact_hold(held: &[Held], place: &Destination, transaction: &str) -> Option<usize> {
    held.iter().position(|held| {
        held.destination == *place && held.transaction.as_deref() == Some(transaction)
    })
}

/// Why no new sticker may be sent to `place` now, if none may: another send
/// is in progress, an earlier one here is unresolved, or too many are.
fn send_blocker(busy: bool, held: &[Held], place: &Destination) -> Option<&'static str> {
    if busy {
        Some(BUSY_NOTICE)
    } else if held.iter().any(|held| held.destination == *place) {
        Some(HELD_NOTICE)
    } else if held.len() >= MAX_HOLDS {
        Some(FULL_NOTICE)
    } else {
        None
    }
}

fn hold_status(hold: Hold) -> &'static str {
    match hold {
        Hold::Enqueue => UNCERTAIN_STATUS,
        Hold::Lost => LOST_STATUS,
        Hold::Checking => CHECKING_STATUS,
        Hold::Gone => GONE_STATUS,
    }
}

/// The send status text: the hold shown here, then the tracked send.
fn strip_status(hold: Option<Hold>, tracked: Option<&str>) -> String {
    match (hold.map(hold_status), tracked) {
        (Some(hold), Some(tracked)) => format!("{hold}\n{tracked}"),
        (Some(text), None) | (None, Some(text)) => text.into(),
        (None, None) => String::new(),
    }
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

/// Status of the tracked send after one queue update: text, cancel, retry
/// and phase. A wedged request is retried in place, never sent again. A lost
/// outcome is never reported as finished: the request is held instead.
fn update_status(update: QueuedSendUpdate) -> (&'static str, bool, bool, SendPhase) {
    match update {
        QueuedSendUpdate::Sent => ("Sticker sent.", false, false, SendPhase::Finished),
        QueuedSendUpdate::Retained { recoverable: true } => (
            "Sticker not sent yet. It will retry automatically.",
            true,
            false,
            SendPhase::Observed,
        ),
        QueuedSendUpdate::Retained { recoverable: false } => (
            "Sticker could not be sent. Retry it or cancel it.",
            true,
            true,
            SendPhase::Observed,
        ),
        QueuedSendUpdate::Cancelled => ("Sticker cancelled.", false, false, SendPhase::Finished),
        QueuedSendUpdate::Unknown => (CHECKING_STATUS, false, false, SendPhase::Lost),
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

fn sending_status(label: &str) -> String {
    if label.is_empty() {
        "Sending sticker…".into()
    } else {
        format!("Sending “{label}”…")
    }
}

/// A sticker found in the queue, for example from before a restart. When
/// more were queued than were read, the count is a floor.
fn pending_status(label: &str, wedged: bool, others: usize, truncated: bool) -> String {
    let mut text = if label.is_empty() {
        String::from("A sticker is still queued here.")
    } else {
        format!("A sticker is still queued here: “{label}”.")
    };
    if wedged {
        text.push_str(" It could not be sent; retry or cancel it.");
    }
    if truncated {
        text.push_str(&format!(
            " At least {others} more queued here will send in order."
        ));
    } else if others > 0 {
        text.push_str(&format!(" {others} more queued here will send in order."));
    }
    text
}

impl<B: StickerBackend> StickerUi<B> {
    fn install(ui: &AppWindow, backend: Arc<B>, runtime: Handle, timing: Timing) -> Arc<Self> {
        let owner = Arc::new(Self {
            backend,
            runtime,
            timing,
            state: Mutex::new(State::default()),
            sessions: AtomicU64::new(0),
            previews: AtomicU64::new(0),
            sends: AtomicU64::new(0),
            holds: AtomicU64::new(0),
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

    fn lock(&self) -> MutexGuard<'_, State<B>> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Republish on the UI thread; publication re-checks every binding.
    fn schedule_publish(self: &Arc<Self>, weak: &slint::Weak<AppWindow>) {
        let owner = self.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| owner.publish(&ui));
    }

    /// Run `act` only while the open picker still belongs to the visible
    /// destination, its composer and its selection. A stale picker is closed
    /// instead, before anything is fetched, consented to or queued for it.
    fn admit<R>(&self, ui: &AppWindow, act: impl FnOnce(&mut State<B>) -> Option<R>) -> Option<R> {
        let mut state = self.lock();
        match state.picker.as_ref().map(|open| picker_valid(ui, open)) {
            Some(true) => return act(&mut state),
            Some(false) => {}
            None => return None,
        }
        let stale = state.picker.take();
        drop(state);
        if let Some(picker) = stale {
            picker.stop.stop();
        }
        self.publish(ui);
        None
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
        let Some(selection) = self.backend.selection(&destination.account) else {
            return;
        };
        let session = next_id(&self.sessions);
        let stop = Arc::new(Stop::default());
        let (jobs, lane) = watch::channel(None);
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
            jobs,
            stop: stop.clone(),
        });
        ui.set_sticker_query_text(SharedString::new());
        ui.set_sticker_pack_index(0);
        self.publish(ui);
        let weak = ui.as_weak();
        let work = Arc::new(PreviewOwner {
            owner: self.clone(),
            weak: weak.clone(),
        });
        let slot = self.worker.clone();
        let lane = pump_previews(work, lane, stop.clone(), slot, self.timing);
        self.runtime.spawn(lane);
        let owner = self.clone();
        self.runtime.spawn(async move {
            owner
                .load(session, selection, destination, stop, weak)
                .await;
        });
    }

    async fn load(
        self: Arc<Self>,
        session: u64,
        selection: B::Selection,
        destination: Destination,
        stop: Arc<Stop>,
        weak: slint::Weak<AppWindow>,
    ) {
        let (backend, thread) = (&self.backend, destination.thread.as_deref());
        let request = backend.catalog(&selection, &destination.room, thread);
        let built = tokio::select! {
            biased;
            _ = stop.wait() => return,
            _ = selection.revoked() => {
                self.schedule_publish(&weak);
                return;
            }
            built = tokio::time::timeout(self.timing.catalog, request) => built,
        };
        let phase = match built {
            Ok(Ok(catalog)) => Phase::Ready(Arc::new(catalog)),
            Ok(Err(error)) => Phase::Failed(catalog_error(&error)),
            Err(_) => Phase::Failed(
                "Loading sticker packs timed out. Close the picker and try again.".into(),
            ),
        };
        let open = self.adopt(session, phase, String::new());
        self.schedule_publish(&weak);
        if !open {
            return;
        }
        // Held requests here whose transaction is known are looked up again,
        // and a sticker the queue still holds here, for example from before a
        // restart, is observed in place. Neither is ever sent again.
        self.recheck_held(&selection, &destination, &weak);
        self.find_pending(&selection, &destination, &weak).await;
        self.request_preview();
        self.schedule_publish(&weak);
        self.watch(session, selection, stop, weak).await;
    }

    /// Install a catalog (or failure) in the still-open picker of `session`.
    /// A replaced catalog loses its consent, choice and preview.
    fn adopt(&self, session: u64, phase: Phase<B::Catalog>, notice: String) -> bool {
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
        picker.clear_preview();
        picker.refilter();
        true
    }

    /// While the picker is open, compare its catalog with the live sources:
    /// a changed, redacted, disabled, newly enabled or departed pack replaces
    /// the catalog and revokes the choice, preview and consent made on it.
    async fn watch(
        self: &Arc<Self>,
        session: u64,
        selection: B::Selection,
        stop: Arc<Stop>,
        weak: slint::Weak<AppWindow>,
    ) {
        let mut updates = self.backend.room_updates(selection.account_id()).await;
        loop {
            tokio::select! {
                biased;
                _ = stop.wait() => return,
                _ = selection.revoked() => {
                    self.schedule_publish(&weak);
                    return;
                }
                _ = tokio::time::sleep(self.timing.watch_interval) => {}
                _ = next_room_update(&mut updates) => {}
            }
            let catalog = {
                let state = self.lock();
                let picker = state.picker.as_ref();
                match picker.filter(|picker| picker.session == session) {
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
                    self.timing.catalog,
                    self.backend.refresh(&catalog),
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
                        self.request_preview();
                    }
                    self.schedule_publish(&weak);
                }
                Ok(Err(error)) => {
                    let failed = Phase::Failed(catalog_error(&error));
                    self.adopt(session, failed, String::new());
                    self.schedule_publish(&weak);
                    if matches!(error, MatrixCoreError::SelectedAccountChanged) {
                        return;
                    }
                }
            }
            tokio::select! {
                biased;
                _ = stop.wait() => return,
                _ = tokio::time::sleep(self.timing.watch_minimum) => {}
            }
        }
    }

    fn search(self: &Arc<Self>, ui: &AppWindow, query: &str) {
        let admitted = self.admit(ui, |state| {
            let picker = state.picker.as_mut()?;
            picker.query = bounded_query(query);
            picker.notice.clear();
            picker.refilter();
            Some(())
        });
        if admitted.is_some() {
            self.request_preview();
        }
        self.publish(ui);
    }

    fn navigate(self: &Arc<Self>, ui: &AppWindow, offset: i32) {
        let admitted = self.admit(ui, |state| {
            let picker = state
                .picker
                .as_mut()
                .filter(|picker| !picker.rows.is_empty())?;
            let length = picker.rows.len() as i64;
            let current = picker.highlighted.map_or(-1, |row| row as i64);
            let next = if current < 0 {
                if offset < 0 { length - 1 } else { 0 }
            } else {
                (current + i64::from(offset.signum())).rem_euclid(length)
            };
            picker.highlighted = Some(next as usize);
            picker.notice.clear();
            Some(())
        });
        if admitted.is_some() {
            self.request_preview();
        }
        self.publish(ui);
    }

    fn choose_pack(self: &Arc<Self>, ui: &AppWindow, index: i32) {
        let admitted = self.admit(ui, |state| {
            let picker = state.picker.as_mut()?;
            let packs = picker.catalog().map_or(0, |catalog| catalog.packs().len());
            picker.pack = usize::try_from(index)
                .ok()
                .and_then(|index| index.checked_sub(1))
                .filter(|pack| *pack < packs);
            picker.notice.clear();
            picker.refilter();
            Some(())
        });
        if admitted.is_some() {
            self.request_preview();
        }
        self.publish(ui);
    }

    fn highlight(self: &Arc<Self>, ui: &AppWindow, key: &str) {
        let admitted = self.admit(ui, |state| {
            let picker = state.picker.as_mut()?;
            let row = parse_key(key, picker.session, &picker.rows)?;
            picker.highlighted = Some(row);
            picker.notice.clear();
            Some(())
        });
        if admitted.is_some() {
            self.request_preview();
        }
        self.publish(ui);
    }

    fn consent(self: &Arc<Self>, ui: &AppWindow, allowed: bool) {
        if !allowed {
            // Cancelling the consent prompt closes the picker; nothing loads.
            self.close(ui, true);
            return;
        }
        let admitted = self.admit(ui, |state| {
            let picker = state.picker.as_mut()?;
            let catalog = picker.catalog().cloned()?;
            picker.consent = Some(catalog.public_media_consent());
            picker.notice.clear();
            picker.clear_preview();
            Some(())
        });
        if admitted.is_some() {
            self.request_preview();
        }
        self.publish(ui);
    }

    /// Hand the highlighted sticker's preview to the picker's latest-only
    /// lane, replacing any earlier request. Without consent nothing is wanted
    /// and nothing is fetched. Callers have admitted the command first.
    fn request_preview(&self) {
        let mut state = self.lock();
        let Some(picker) = state.picker.as_mut() else {
            return;
        };
        let Some(catalog) = picker.catalog().cloned() else {
            return;
        };
        let Some(entry) = picker.current_entry() else {
            picker.clear_preview();
            picker.preview.status = "Choose a sticker to see it.".into();
            return;
        };
        if picker.preview.entry == Some(entry) && picker.preview.request != 0 {
            return;
        }
        let request = next_id(&self.previews);
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
        let job = consented.then(|| {
            Arc::new(PreviewJob {
                session: picker.session,
                request,
                catalog,
                entry,
                consent: picker.consent,
            })
        });
        picker.jobs.send_replace(job);
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
            picker.jobs.send_replace(None);
        }
    }

    fn finish_preview(&self, session: u64, request: u64, result: PreviewResult) {
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
        let prepared = self.admit(ui, |state| {
            let busy = state.enqueuing || state.tracked.as_ref().is_some_and(blocks_send);
            let picker = state.picker.as_mut()?;
            let row = parse_key(key, picker.session, &picker.rows)?;
            let catalog = picker.catalog().cloned()?;
            picker.highlighted = Some(row);
            let entry = picker.rows[row];
            if let Some(notice) = send_blocker(busy, &state.held, &picker.destination) {
                picker.notice = notice.into();
                return Some(None);
            }
            if !picker.consented() {
                picker.notice = "Allow sticker images here before sending.".into();
                return Some(None);
            }
            let label = catalog
                .entries()
                .get(entry)
                .map(|entry| entry.label.clone())
                .unwrap_or_default();
            let prepared = Prepared::<B> {
                session: picker.session,
                catalog,
                entry,
                consent: picker.consent,
                selection: picker.selection.clone(),
                destination: picker.destination.clone(),
                label,
            };
            state.enqueuing = true;
            Some(Some(prepared))
        });
        self.publish(ui);
        let Some(Some(prepared)) = prepared else {
            return;
        };
        let owner = self.clone();
        let weak = ui.as_weak();
        self.runtime.spawn(async move {
            // The core re-validates the selection, room, consent and pack
            // source after every await and holds the switch lock to enqueue.
            let queued = owner
                .backend
                .send(&prepared.catalog, prepared.entry, prepared.consent)
                .await;
            match queued {
                Ok(queued) => {
                    // Recorded under the same lock that ends the enqueue, so
                    // no request found in the queue meanwhile can displace it.
                    let status = sending_status(&prepared.label);
                    let (send, commands) = {
                        let mut state = owner.lock();
                        state.enqueuing = false;
                        owner.record(
                            &mut state,
                            &queued,
                            &prepared.selection,
                            prepared.destination,
                            status,
                            false,
                        )
                    };
                    owner.observe(queued, prepared.selection, send, commands, false, &weak);
                    owner.close_session(prepared.session, &weak);
                }
                Err(MatrixCoreError::ComposedSendUncertain) => {
                    // There is no transaction to look for, so only the user's
                    // confirmation lifts this hold; nothing is retried.
                    {
                        let mut state = owner.lock();
                        state.enqueuing = false;
                        owner.hold(&mut state, prepared.destination, None, Hold::Enqueue);
                    }
                    owner.close_session(prepared.session, &weak);
                }
                Err(error) => {
                    {
                        let mut state = owner.lock();
                        state.enqueuing = false;
                        if let Some(picker) = state
                            .picker
                            .as_mut()
                            .filter(|picker| picker.session == prepared.session)
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

    /// Close the picker of `session` on the UI thread, if it is still open:
    /// the choice it was made from is spent.
    fn close_session(self: &Arc<Self>, session: u64, weak: &slint::Weak<AppWindow>) {
        let owner = self.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let open = owner
                .lock()
                .picker
                .as_ref()
                .is_some_and(|picker| picker.session == session);
            if open {
                owner.close(&ui, true);
            } else {
                owner.publish(&ui);
            }
        });
    }

    /// Record `queued` as the one tracked send, under the caller's lock.
    fn record(
        &self,
        state: &mut State<B>,
        queued: &B::Queued,
        selection: &B::Selection,
        destination: Destination,
        status: String,
        wedged: bool,
    ) -> (u64, mpsc::Receiver<Command>) {
        let send = next_id(&self.sends);
        let (commands, receiver) = mpsc::channel(1);
        state.tracked = Some(Tracked {
            send,
            destination,
            selection: selection.clone(),
            transaction: queued.transaction_id().to_owned(),
            status,
            phase: SendPhase::Observed,
            can_cancel: true,
            can_retry: wedged,
            commands: Some(commands),
        });
        (send, receiver)
    }

    /// Observe one recorded sticker by its transaction until it finishes or
    /// the user cancels it. When its outcome is lost (the update stream ended
    /// or lagged, or the selection ended) it is held with its transaction.
    /// The SDK request itself persists in the account's queue either way.
    fn observe(
        self: &Arc<Self>,
        mut queued: B::Queued,
        selection: B::Selection,
        send: u64,
        mut receiver: mpsc::Receiver<Command>,
        wedged: bool,
        weak: &slint::Weak<AppWindow>,
    ) {
        self.schedule_publish(weak);
        let owner = self.clone();
        let weak = weak.clone();
        let sent_status = self.timing.sent_status;
        self.runtime.spawn(async move {
            let mut wedged = wedged;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = selection.revoked() => {
                        owner.lose(send, &selection, &weak);
                        return;
                    }
                    update = queued.next_update() => Event::Update(update),
                    command = receiver.recv() => Event::Command(command),
                };
                match event {
                    Event::Update(update) => {
                        let (text, can_cancel, can_retry, phase) = update_status(update);
                        if phase == SendPhase::Lost {
                            owner.lose(send, &selection, &weak);
                            return;
                        }
                        wedged = can_retry;
                        owner.set_status(send, text, can_cancel, can_retry, phase);
                        owner.schedule_publish(&weak);
                        if phase == SendPhase::Finished {
                            if update == QueuedSendUpdate::Sent {
                                tokio::time::sleep(sent_status).await;
                                owner.clear_finished(send);
                                owner.schedule_publish(&weak);
                            }
                            return;
                        }
                    }
                    Event::Command(Some(Command::Cancel)) => {
                        // A successful abort is confirmed by the queue update.
                        let (text, phase, again) = match queued.abort().await {
                            Ok(true) => ("Cancelling sticker…", SendPhase::Observed, false),
                            Ok(false) => (ALREADY_SENT, SendPhase::Finished, false),
                            Err(_) => (CANCEL_FAILED, SendPhase::Observed, true),
                        };
                        owner.set_status(send, text, again, again && wedged, phase);
                        owner.schedule_publish(&weak);
                        if phase == SendPhase::Finished {
                            return;
                        }
                    }
                    Event::Command(Some(Command::Retry)) => {
                        let (text, retry) = match queued.retry_wedged().await {
                            Ok(()) => ("Retrying sticker…", false),
                            Err(_) => (RETRY_FAILED, true),
                        };
                        wedged = retry;
                        owner.set_status(send, text, true, retry, SendPhase::Observed);
                        owner.schedule_publish(&weak);
                    }
                    Event::Command(None) => {
                        // The tracked send was held or replaced elsewhere.
                        owner.lose(send, &selection, &weak);
                        return;
                    }
                }
            }
        });
    }

    /// Observe the newest sticker the queue still holds for `destination`,
    /// when nothing else is observed: it keeps its original transaction. It
    /// lifts a hold only when it is exactly that hold's request.
    async fn find_pending(
        self: &Arc<Self>,
        selection: &B::Selection,
        destination: &Destination,
        weak: &slint::Weak<AppWindow>,
    ) {
        let (backend, thread) = (&self.backend, destination.thread.as_deref());
        let lookup = backend.pending(selection, &destination.room, thread);
        let found = tokio::select! {
            biased;
            _ = selection.revoked() => return,
            found = tokio::time::timeout(self.timing.catalog, lookup) => found,
        };
        let Ok(Ok(Some(pending))) = found else {
            return;
        };
        let Pending {
            label,
            wedged,
            others,
            truncated,
            queued,
        } = pending;
        let status = pending_status(&label, wedged, others, truncated);
        let (send, commands) = {
            let mut state = self.lock();
            self.retire_ended(&mut state);
            let transaction = queued.transaction_id();
            let exact = exact_hold(&state.held, destination, transaction);
            let room = exact.is_some() || state.held.len() < MAX_HOLDS;
            let free = !state.enqueuing && may_adopt(state.tracked.as_ref(), transaction);
            if !room || !free {
                return;
            }
            if let Some(index) = exact {
                state.held.remove(index);
            }
            self.record(
                &mut state,
                &queued,
                selection,
                destination.clone(),
                status,
                wedged,
            )
        };
        self.observe(queued, selection.clone(), send, commands, wedged, weak);
    }

    /// Hold a send under the caller's lock and return the hold's id.
    fn hold(
        &self,
        state: &mut State<B>,
        destination: Destination,
        transaction: Option<String>,
        hold: Hold,
    ) -> u64 {
        let id = next_id(&self.holds);
        state.held.push(Held {
            id,
            destination,
            transaction,
            hold,
        });
        id
    }

    /// Under the caller's lock, hold the observed tracked request `send` with
    /// its transaction. `None` when `send` is no longer the observed request.
    fn retire(&self, state: &mut State<B>, send: u64) -> Option<u64> {
        match &state.tracked {
            Some(tracked) if tracked.send == send && tracked.phase == SendPhase::Observed => {}
            _ => return None,
        }
        let tracked = state.tracked.take()?;
        let transaction = Some(tracked.transaction);
        Some(self.hold(state, tracked.destination, transaction, Hold::Lost))
    }

    /// Under the caller's lock: a request observed under an ended selection
    /// lost its outcome and is held; a finished one is dropped.
    fn retire_ended(&self, state: &mut State<B>) {
        let ended = state
            .tracked
            .as_ref()
            .filter(|tracked| !tracked.selection.is_current())
            .map(|tracked| tracked.send);
        if let Some(send) = ended
            && self.retire(state, send).is_none()
        {
            state.tracked = None;
        }
    }

    /// The tracked request `send` lost its outcome. It is held with its
    /// transaction and, under a current selection, looked up again at once.
    fn lose(self: &Arc<Self>, send: u64, selection: &B::Selection, weak: &slint::Weak<AppWindow>) {
        let held = self.retire(&mut self.lock(), send);
        if let Some(id) = held
            && selection.is_current()
        {
            self.reconcile(id, selection.clone(), weak);
        }
        self.schedule_publish(weak);
    }

    /// Look up every request held here whose transaction is known and that is
    /// not already being looked up, with this picker's selection.
    fn recheck_held(
        self: &Arc<Self>,
        selection: &B::Selection,
        destination: &Destination,
        weak: &slint::Weak<AppWindow>,
    ) {
        let ids: Vec<u64> = self
            .lock()
            .held
            .iter()
            .filter(|held| held.destination == *destination && held.hold == Hold::Lost)
            .map(|held| held.id)
            .collect();
        for id in ids {
            self.reconcile(id, selection.clone(), weak);
        }
    }

    /// Look the held request `id` up by its exact transaction, a bounded
    /// number of times while `selection` stays current. Still queued, it is
    /// observed again in place; gone from the queue, it was sent or cancelled
    /// and only the user can confirm which. Nothing is ever sent again.
    fn reconcile(
        self: &Arc<Self>,
        id: u64,
        selection: B::Selection,
        weak: &slint::Weak<AppWindow>,
    ) {
        let Some((destination, transaction)) = self.begin_check(id) else {
            return;
        };
        let owner = self.clone();
        let task_weak = weak.clone();
        self.runtime.spawn(async move {
            let hold = owner
                .check_queue(id, &selection, &destination, &transaction, &task_weak)
                .await;
            owner.end_check(id, hold);
            owner.schedule_publish(&task_weak);
        });
        self.schedule_publish(weak);
    }

    /// Mark the held request `id` as being looked up, unless it already is,
    /// has no transaction or is known to be gone.
    fn begin_check(&self, id: u64) -> Option<(Destination, String)> {
        let mut state = self.lock();
        let held = state
            .held
            .iter_mut()
            .find(|held| held.id == id && held.hold == Hold::Lost)?;
        let transaction = held.transaction.clone()?;
        held.hold = Hold::Checking;
        Some((held.destination.clone(), transaction))
    }

    /// Look the held transaction up. `None` once it is observed again or the
    /// hold was confirmed meanwhile; otherwise the hold it ends in.
    async fn check_queue(
        self: &Arc<Self>,
        id: u64,
        selection: &B::Selection,
        destination: &Destination,
        transaction: &str,
        weak: &slint::Weak<AppWindow>,
    ) -> Option<Hold> {
        let room = &destination.room;
        for attempt in 0..self.timing.checks {
            if attempt > 0 {
                tokio::select! {
                    biased;
                    _ = selection.revoked() => return Some(Hold::Lost),
                    _ = tokio::time::sleep(self.timing.recheck) => {}
                }
            }
            if !self.is_held(id) {
                return None;
            }
            let lookup = self.backend.queued(selection, room, transaction);
            let found = tokio::select! {
                biased;
                _ = selection.revoked() => return Some(Hold::Lost),
                found = tokio::time::timeout(self.timing.catalog, lookup) => found,
            };
            match found {
                Ok(Ok(Some(pending))) => {
                    if self.observe_again(id, pending, selection, destination, weak) {
                        return None;
                    }
                }
                Ok(Ok(None)) => return Some(Hold::Gone),
                Ok(Err(_)) | Err(_) => {}
            }
        }
        Some(Hold::Lost)
    }

    /// Observe the exact held request again in place, lifting its hold, when
    /// the hold still exists and the tracked slot is free.
    fn observe_again(
        self: &Arc<Self>,
        id: u64,
        pending: Pending<B::Queued>,
        selection: &B::Selection,
        destination: &Destination,
        weak: &slint::Weak<AppWindow>,
    ) -> bool {
        let status = pending_status(&pending.label, pending.wedged, 0, false);
        let wedged = pending.wedged;
        let queued = pending.queued;
        let (send, commands) = {
            let mut state = self.lock();
            self.retire_ended(&mut state);
            let transaction = queued.transaction_id();
            let exact = exact_hold(&state.held, destination, transaction);
            let free = !state.enqueuing && may_adopt(state.tracked.as_ref(), transaction);
            let Some(index) = exact.filter(|index| free && state.held[*index].id == id) else {
                return false;
            };
            state.held.remove(index);
            self.record(
                &mut state,
                &queued,
                selection,
                destination.clone(),
                status,
                wedged,
            )
        };
        self.observe(queued, selection.clone(), send, commands, wedged, weak);
        true
    }

    fn is_held(&self, id: u64) -> bool {
        self.lock().held.iter().any(|held| held.id == id)
    }

    /// Record how the lookup of `id` ended, unless its hold was lifted.
    fn end_check(&self, id: u64, hold: Option<Hold>) {
        let mut state = self.lock();
        if let Some(hold) = hold
            && let Some(held) = state.held.iter_mut().find(|held| held.id == id)
        {
            held.hold = hold;
        }
    }

    fn set_status(
        &self,
        send: u64,
        text: &str,
        can_cancel: bool,
        can_retry: bool,
        phase: SendPhase,
    ) {
        let mut state = self.lock();
        if let Some(tracked) = state
            .tracked
            .as_mut()
            .filter(|tracked| tracked.send == send)
        {
            let finished = phase == SendPhase::Finished;
            tracked.status = text.into();
            tracked.phase = phase;
            tracked.can_cancel = can_cancel && !finished;
            tracked.can_retry = can_retry && !finished;
            if finished {
                tracked.commands = None;
            }
        }
    }

    fn clear_finished(&self, send: u64) {
        let mut state = self.lock();
        if state
            .tracked
            .as_ref()
            .is_some_and(|tracked| tracked.send == send && tracked.phase == SendPhase::Finished)
        {
            state.tracked = None;
        }
    }

    /// Admit at most one cancel or retry at a time: the controls are disabled
    /// as the command is accepted, and the channel holds one command.
    fn command(&self, ui: &AppWindow, command: Command) {
        {
            let mut state = self.lock();
            let shown = state.shown_send;
            let Some(tracked) = state
                .tracked
                .as_mut()
                .filter(|tracked| shown == Some(tracked.send) && tracked_visible(ui, tracked))
            else {
                drop(state);
                self.publish(ui);
                return;
            };
            let permitted = match command {
                Command::Cancel => tracked.can_cancel,
                Command::Retry => tracked.can_retry,
            };
            let accepted = permitted
                && tracked
                    .commands
                    .as_ref()
                    .is_some_and(|commands| commands.try_send(command).is_ok());
            if accepted {
                tracked.can_cancel = false;
                tracked.can_retry = false;
                tracked.status = match command {
                    Command::Cancel => "Cancelling sticker…",
                    Command::Retry => "Retrying sticker…",
                }
                .into();
            }
        }
        self.publish(ui);
    }

    /// Lift the hold the status showed, after the user checked the
    /// conversation, or clear a finished status. Only what was shown, and is
    /// still shown here, is acted on; an observed send is never dismissed.
    fn dismiss_status(&self, ui: &AppWindow) {
        {
            let mut state = self.lock();
            let (hold, send) = (state.shown_hold, state.shown_send);
            let index = state
                .held
                .iter()
                .position(|held| Some(held.id) == hold && shown_at(ui, &held.destination));
            let finished = state.tracked.as_ref().is_some_and(|tracked| {
                Some(tracked.send) == send
                    && tracked.phase == SendPhase::Finished
                    && tracked_visible(ui, tracked)
            });
            if let Some(index) = index {
                state.held.remove(index);
            } else if hold.is_none() && finished {
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
            self.retire_ended(&mut state);
            stopped
        };
        if let Some(picker) = stale {
            picker.stop.stop();
        }
        let mut state = self.lock();
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
                        (items, packs, ready_status(&**catalog, &picker.rows))
                    }
                };
                let pack_index = picker.pack.map_or(0, |pack| pack as i32 + 1);
                let index = picker.highlighted.map_or(-1, |row| row as i32);
                ui.set_sticker_threaded(picker.destination.threaded());
                ui.set_sticker_items(ModelRc::new(VecModel::from(items)));
                ui.set_sticker_packs(ModelRc::new(VecModel::from(packs)));
                ui.set_sticker_pack_index(pack_index);
                ui.set_sticker_index(index);
                ui.set_sticker_phase(match picker.phase {
                    Phase::Loading => 0,
                    Phase::Ready(_) => 1,
                    Phase::Failed(_) => 2,
                });
                ui.set_sticker_status(status.into());
                ui.set_sticker_notice(picker.notice.clone().into());
                ui.set_sticker_needs_consent(picker.catalog().is_some() && !picker.consented());
                let busy = state.enqueuing || state.tracked.as_ref().is_some_and(blocks_send);
                let blocked = send_blocker(busy, &state.held, &picker.destination);
                ui.set_sticker_can_send(
                    picker.catalog().is_some()
                        && picker.current_entry().is_some()
                        && picker.consented()
                        && blocked.is_none(),
                );
                match &picker.preview.pixels {
                    Some(pixels) => {
                        let image = slint::Image::from_rgba8(pixels.clone());
                        ui.set_sticker_preview_image(image);
                        ui.set_sticker_preview_loaded(true);
                    }
                    None => {
                        ui.set_sticker_preview_image(slint::Image::default());
                        ui.set_sticker_preview_loaded(false);
                    }
                }
                let preview_status = picker.preview.status.clone();
                ui.set_sticker_preview_status(preview_status.into());
                ui.set_sticker_open(true);
            }
        }
        // One status per visible place: the oldest hold shown here, and the
        // tracked send when it belongs to the same place.
        let held = state
            .held
            .iter()
            .find(|held| shown_at(ui, &held.destination));
        let place = held.map(|held| &held.destination);
        let tracked = state
            .tracked
            .as_ref()
            .filter(|tracked| tracked_visible(ui, tracked))
            .filter(|tracked| place.is_none_or(|place| *place == tracked.destination));
        let text = strip_status(
            held.map(|held| held.hold),
            tracked.map(|tracked| tracked.status.as_str()),
        );
        let can_cancel = tracked.is_some_and(|tracked| tracked.can_cancel);
        let can_retry = tracked.is_some_and(|tracked| tracked.can_retry);
        let threaded = place
            .or(tracked.map(|tracked| &tracked.destination))
            .map(Destination::threaded);
        let shown = (
            held.map(|held| held.id),
            tracked.map(|tracked| tracked.send),
        );
        ui.set_sticker_send_status(text.into());
        ui.set_sticker_send_can_cancel(can_cancel);
        ui.set_sticker_send_can_retry(can_retry);
        ui.set_sticker_send_uncertain(shown.0.is_some());
        ui.set_sticker_send_threaded(threaded.unwrap_or(false));
        ui.set_sticker_send_visible(threaded.is_some());
        (state.shown_hold, state.shown_send) = shown;
    }
}

fn ready_status<C: Catalog>(catalog: &C, rows: &[usize]) -> String {
    if catalog.entries().is_empty() {
        let mut text = String::from(
            "No sticker packs here yet. Packs come from this room and from packs you enabled in rooms you have joined.",
        );
        if catalog.unavailable_packs() > 0 {
            text.push_str(" Some enabled packs are in rooms not joined on this device.");
        }
        return text;
    }
    if rows.is_empty() {
        return "No stickers match your search.".into();
    }
    let mut text = format!(
        "{} sticker{} in {} pack{}.",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        catalog.packs().len(),
        if catalog.packs().len() == 1 { "" } else { "s" },
    );
    if rows.len() == MAX_ROWS {
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

async fn next_room_update(updates: &mut Option<broadcast::Receiver<RoomUpdates>>) {
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

/// Short pauses so tests observe the same ordering without waiting long.
#[cfg(test)]
const TEST_TIMING: Timing = Timing {
    catalog: Duration::from_secs(5),
    preview: Duration::from_secs(5),
    preview_delay: Duration::from_millis(5),
    worker_wait: Duration::from_millis(300),
    watch_interval: Duration::from_millis(50),
    watch_minimum: Duration::from_millis(10),
    sent_status: Duration::from_millis(50),
    recheck: Duration::from_millis(20),
    checks: 50,
};

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::StickerPackSource;
    use std::sync::atomic::AtomicUsize;

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
        assert!(filter(&entries, &packs, "star", Some(0)).is_empty());
        let many: Vec<_> = (0..MAX_ROWS + 9).map(|_| entry("x", "x", 0)).collect();
        assert_eq!(filter(&many, &packs, "", None).len(), MAX_ROWS);
    }

    #[test]
    fn keys_belong_to_one_picker_session_and_a_shown_row() {
        let rows = [4, 9];
        assert_eq!(parse_key(&item_key(7, 9), 7, &rows), Some(1));
        assert_eq!(parse_key(&item_key(6, 9), 7, &rows), None);
        assert_eq!(parse_key(&item_key(7, 5), 7, &rows), None);
        for key in [
            "",
            "7",
            "7:",
            ":9",
            "7:x",
            "x:9",
            "7:9:1",
            "mxc://example.org/a",
        ] {
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
        let (text, cancel, retry, phase) = update_status(QueuedSendUpdate::Sent);
        assert_eq!((text, cancel, retry), ("Sticker sent.", false, false));
        assert_eq!(phase, SendPhase::Finished);
        let (_, cancel, retry, phase) =
            update_status(QueuedSendUpdate::Retained { recoverable: true });
        assert!(cancel && !retry && phase == SendPhase::Observed);
        let (_, cancel, retry, phase) =
            update_status(QueuedSendUpdate::Retained { recoverable: false });
        assert!(cancel && retry && phase == SendPhase::Observed);
        let (_, cancel, retry, phase) = update_status(QueuedSendUpdate::Cancelled);
        assert!(!cancel && !retry && phase == SendPhase::Finished);
        // A lost outcome is held with its transaction, never reported as
        // finished, and offers neither cancel nor retry.
        let (text, cancel, retry, phase) = update_status(QueuedSendUpdate::Unknown);
        assert_eq!((text, cancel, retry), (CHECKING_STATUS, false, false));
        assert_eq!(phase, SendPhase::Lost);
    }

    fn held(id: u64, at: &Destination, transaction: Option<&str>, hold: Hold) -> Held {
        Held {
            id,
            destination: at.clone(),
            transaction: transaction.map(str::to_owned),
            hold,
        }
    }

    #[test]
    fn holds_block_their_destination_and_only_their_exact_request_lifts_one() {
        let (room, thread, other) = (place(None), place(Some("$root")), place(Some("$other")));
        let holds = [
            held(1, &room, None, Hold::Enqueue),
            held(2, &thread, Some("txn-lost"), Hold::Gone),
        ];
        assert_eq!(send_blocker(true, &[], &room), Some(BUSY_NOTICE));
        assert_eq!(send_blocker(false, &holds, &room), Some(HELD_NOTICE));
        assert_eq!(send_blocker(false, &holds, &thread), Some(HELD_NOTICE));
        assert_eq!(send_blocker(false, &holds, &other), None);
        // A full set of holds stops every new send; none is dropped for room.
        let full: Vec<Held> = (0..MAX_HOLDS as u64)
            .map(|id| held(id, &thread, None, Hold::Enqueue))
            .collect();
        assert_eq!(send_blocker(false, &full, &other), Some(FULL_NOTICE));
        // An enqueue that never returned has no transaction, so no queued
        // request lifts it; a lost outcome is lifted only by its own request.
        assert_eq!(exact_hold(&holds, &room, "txn-lost"), None);
        assert_eq!(exact_hold(&holds, &room, "txn-newest"), None);
        assert_eq!(exact_hold(&holds, &thread, "txn-newest"), None);
        assert_eq!(exact_hold(&holds, &other, "txn-lost"), None);
        assert_eq!(exact_hold(&holds, &thread, "txn-lost"), Some(1));
        let both = strip_status(Some(Hold::Gone), Some("Sticker sent."));
        assert_eq!(both, format!("{GONE_STATUS}\nSticker sent."));
        assert_eq!(strip_status(None, None), "");
        for hold in [Hold::Enqueue, Hold::Lost, Hold::Checking, Hold::Gone] {
            assert!(hold_status(hold).contains("confirm"), "{hold:?}");
        }
    }

    #[test]
    fn failures_and_found_requests_use_fixed_text_without_core_details() {
        let detail = MatrixCoreError::Sdk("PRIVATE mxc://example.org/secret".into());
        assert!(!send_failure(&detail).contains("PRIVATE"));
        assert!(!catalog_error(&detail).contains("PRIVATE"));
        assert!(!download_failure(&detail).contains("PRIVATE"));
        assert!(send_failure(&MatrixCoreError::StickerConsentRequired).contains("Allow"));
        assert!(send_failure(&MatrixCoreError::StickerUnavailable).contains("again"));
        let found = pending_status("Sleeping owl", true, 2, false);
        assert!(found.contains("still queued"));
        assert!(found.contains("retry or cancel"));
        assert!(found.contains(" 2 more"));
        let unnamed = pending_status("", false, 0, false);
        assert_eq!(unnamed, "A sticker is still queued here.");
        let truncated = pending_status("Waving fox", false, 255, true);
        assert!(truncated.contains("At least 255 more"));
    }

    #[test]
    fn queries_are_bounded() {
        let query = bounded_query(&"q".repeat(MAX_QUERY_CHARS + 5));
        assert_eq!(query.chars().count(), MAX_QUERY_CHARS);
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
    fn the_decode_slot_is_single_and_released_on_drop() {
        let gate = Arc::new(AtomicBool::new(false));
        let permit = Permit::acquire(&gate).unwrap();
        assert!(Permit::acquire(&gate).is_none());
        drop(permit);
        assert!(Permit::acquire(&gate).is_some());
    }

    /// Counts what the preview lane does. A fetch can be held open, and a
    /// fetch future dropped before it completed is counted as abandoned.
    struct CountingWork {
        current: AtomicBool,
        fetches: AtomicUsize,
        abandoned: AtomicUsize,
        decodes: AtomicUsize,
        finished: Mutex<Vec<u32>>,
        errors: Mutex<Vec<&'static str>>,
        holds: Mutex<Vec<(u32, oneshot::Receiver<()>)>>,
        started: mpsc::UnboundedSender<u32>,
    }

    struct Abandoned<'a> {
        count: &'a AtomicUsize,
        completed: bool,
    }

    impl Drop for Abandoned<'_> {
        fn drop(&mut self) {
            if !self.completed {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl PreviewWork for CountingWork {
        type Job = u32;

        fn current(&self, _: &u32) -> bool {
            self.current.load(Ordering::SeqCst)
        }

        async fn fetch(&self, job: &u32) -> Result<Zeroizing<Vec<u8>>, &'static str> {
            let mut abandoned = Abandoned {
                count: &self.abandoned,
                completed: false,
            };
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let _ = self.started.send(*job);
            let hold = {
                let mut holds = self.holds.lock().unwrap();
                let held = holds.iter().position(|(held, _)| held == job);
                held.map(|index| holds.remove(index).1)
            };
            if let Some(hold) = hold {
                let _ = hold.await;
            }
            abandoned.completed = true;
            Ok(Zeroizing::new(vec![1, 2, 3]))
        }

        fn decode(
            &self,
            permit: Permit,
            _: Zeroizing<Vec<u8>>,
        ) -> Result<oneshot::Receiver<PreviewResult>, &'static str> {
            self.decodes.fetch_add(1, Ordering::SeqCst);
            let (sender, receiver) = oneshot::channel();
            drop(permit);
            let _ = sender.send(Ok(SharedPixelBuffer::new(1, 1)));
            Ok(receiver)
        }

        fn finish(&self, job: &u32, result: PreviewResult) {
            self.finished.lock().unwrap().push(*job);
            if let Err(text) = result {
                self.errors.lock().unwrap().push(text);
            }
        }
    }

    struct Lane {
        work: Arc<CountingWork>,
        started: mpsc::UnboundedReceiver<u32>,
        jobs: watch::Sender<Option<Arc<u32>>>,
        stop: Arc<Stop>,
        slot: Arc<AtomicBool>,
        pump: tokio::task::JoinHandle<()>,
    }

    fn lane(slot: Arc<AtomicBool>) -> Lane {
        let (started, receiver) = mpsc::unbounded_channel();
        let work = Arc::new(CountingWork {
            current: AtomicBool::new(true),
            fetches: AtomicUsize::new(0),
            abandoned: AtomicUsize::new(0),
            decodes: AtomicUsize::new(0),
            finished: Mutex::new(Vec::new()),
            errors: Mutex::new(Vec::new()),
            holds: Mutex::new(Vec::new()),
            started,
        });
        let (jobs, wanted) = watch::channel(None);
        let stop = Arc::new(Stop::default());
        let pump = pump_previews(
            work.clone(),
            wanted,
            stop.clone(),
            slot.clone(),
            TEST_TIMING,
        );
        Lane {
            work,
            started: receiver,
            jobs,
            stop,
            slot,
            pump: tokio::spawn(pump),
        }
    }

    impl Lane {
        fn want(&self, job: u32) {
            self.jobs.send_replace(Some(Arc::new(job)));
        }

        fn hold(&self, job: u32) -> oneshot::Sender<()> {
            let (release, hold) = oneshot::channel();
            self.work.holds.lock().unwrap().push((job, hold));
            release
        }

        async fn next_fetch(&mut self) -> Option<u32> {
            let started = self.started.recv();
            tokio::time::timeout(Duration::from_secs(2), started)
                .await
                .expect("the lane never started a fetch")
        }

        async fn finished(&self, expected: &[u32]) {
            let work = &self.work;
            tokio::time::timeout(Duration::from_secs(2), async {
                while work.finished.lock().unwrap().as_slice() != expected {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the lane did not finish the expected previews");
        }

        fn count(&self, counter: fn(&CountingWork) -> &AtomicUsize) -> usize {
            counter(&self.work).load(Ordering::SeqCst)
        }

        async fn ended(self) {
            tokio::time::timeout(Duration::from_secs(2), self.pump)
                .await
                .expect("the lane outlived its picker")
                .unwrap();
        }
    }

    #[tokio::test]
    async fn rapid_replacements_start_only_the_latest_preview() {
        let mut lane = lane(Arc::default());
        for job in 1..=10 {
            lane.want(job);
        }
        assert_eq!(lane.next_fetch().await, Some(10));
        lane.finished(&[10]).await;
        assert_eq!(lane.count(|work| &work.fetches), 1);
        assert_eq!(lane.count(|work| &work.decodes), 1);
        lane.stop.stop();
        lane.ended().await;
    }

    #[tokio::test]
    async fn a_newer_request_cancels_a_running_fetch_whose_bytes_are_never_decoded() {
        let mut lane = lane(Arc::default());
        let _release = lane.hold(1);
        lane.want(1);
        assert_eq!(lane.next_fetch().await, Some(1));
        lane.want(2);
        assert_eq!(lane.next_fetch().await, Some(2));
        lane.finished(&[2]).await;
        assert_eq!(
            lane.count(|work| &work.abandoned),
            1,
            "the old fetch was dropped"
        );
        assert_eq!(lane.count(|work| &work.fetches), 2);
        assert_eq!(
            lane.count(|work| &work.decodes),
            1,
            "only the wanted job decoded"
        );
        drop(lane.jobs);
        tokio::time::timeout(Duration::from_secs(2), lane.pump)
            .await
            .expect("the lane outlived its picker")
            .unwrap();
    }

    #[tokio::test]
    async fn a_closed_or_changed_context_never_decodes_even_with_a_free_slot() {
        let mut lane = lane(Arc::default());
        let release = lane.hold(1);
        lane.want(1);
        assert_eq!(lane.next_fetch().await, Some(1));
        // The picker's selection or destination changes during the fetch.
        lane.work.current.store(false, Ordering::SeqCst);
        release.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(lane.count(|work| &work.fetches), 1);
        assert_eq!(lane.count(|work| &work.decodes), 0);
        assert!(lane.work.finished.lock().unwrap().is_empty());
        assert!(
            !lane.slot.load(Ordering::SeqCst),
            "the decode slot stayed free"
        );
        // A job wanted while the context is closed is not even fetched.
        lane.want(2);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(lane.count(|work| &work.fetches), 1);
        // Dropping the picker's job sender ends the lane.
        drop(lane.jobs);
        tokio::time::timeout(Duration::from_secs(2), lane.pump)
            .await
            .expect("the lane outlived its picker")
            .unwrap();
    }

    #[tokio::test]
    async fn the_decode_slot_stays_taken_until_the_running_codec_returns() {
        let slot = Arc::new(AtomicBool::new(false));
        // A codec run for an earlier, superseded request still holds the slot.
        let running = Permit::acquire(&slot).unwrap();
        let mut lane = lane(slot.clone());
        lane.want(1);
        assert_eq!(lane.next_fetch().await, Some(1));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(lane.count(|work| &work.decodes), 0, "no second codec run");
        drop(running);
        lane.finished(&[1]).await;
        assert_eq!(lane.count(|work| &work.decodes), 1);
        // A slot that never frees reports busy within the bound.
        let stuck = Permit::acquire(&slot).unwrap();
        lane.want(2);
        assert_eq!(lane.next_fetch().await, Some(2));
        lane.finished(&[1, 2]).await;
        assert_eq!(lane.count(|work| &work.decodes), 1);
        let busy = preview_failure(PreviewError::Busy);
        assert_eq!(lane.work.errors.lock().unwrap().as_slice(), [busy]);
        drop(stuck);
        lane.stop.stop();
        lane.ended().await;
    }
}

/// The real controller over a recording fake core, driven through the
/// generated window callbacks. Run from the single real-window UI test.
#[cfg(test)]
pub(crate) mod controller_tests {
    use super::*;
    use morpheus_matrix::StickerPackSource;
    use slint::Model;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone)]
    struct FakeSelection {
        account: Arc<str>,
        current: Arc<AtomicBool>,
        revocation: Arc<Notify>,
    }

    impl FakeSelection {
        fn new(account: &str) -> Self {
            Self {
                account: account.into(),
                current: Arc::new(AtomicBool::new(true)),
                revocation: Arc::new(Notify::new()),
            }
        }

        fn revoke(&self) {
            self.current.store(false, Ordering::SeqCst);
            self.revocation.notify_waiters();
        }
    }

    impl Selection for FakeSelection {
        fn account_id(&self) -> &str {
            &self.account
        }

        fn is_current(&self) -> bool {
            self.current.load(Ordering::SeqCst)
        }

        async fn revoked(&self) {
            loop {
                let revocation = self.revocation.notified();
                tokio::pin!(revocation);
                revocation.as_mut().enable();
                if !self.is_current() {
                    return;
                }
                revocation.await;
            }
        }
    }

    struct FakeCatalog {
        id: u64,
        encrypted: bool,
        entries: Vec<StickerEntry>,
        packs: Vec<StickerPack>,
    }

    impl Catalog for FakeCatalog {
        type Consent = u64;

        fn entries(&self) -> &[StickerEntry] {
            &self.entries
        }

        fn packs(&self) -> &[StickerPack] {
            &self.packs
        }

        fn skipped(&self) -> usize {
            0
        }

        fn unavailable_packs(&self) -> usize {
            0
        }

        fn requires_public_media_consent(&self) -> bool {
            self.encrypted
        }

        fn public_media_consent(&self) -> u64 {
            self.id
        }
    }

    struct FakeQueued {
        transaction: String,
        updates: mpsc::UnboundedReceiver<QueuedSendUpdate>,
        aborts: Arc<AtomicUsize>,
    }

    impl QueuedSticker for FakeQueued {
        fn transaction_id(&self) -> &str {
            &self.transaction
        }

        async fn next_update(&mut self) -> QueuedSendUpdate {
            self.updates
                .recv()
                .await
                .unwrap_or(QueuedSendUpdate::Unknown)
        }

        async fn abort(&self) -> Result<bool, MatrixCoreError> {
            self.aborts.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }

        async fn retry_wedged(&self) -> Result<(), MatrixCoreError> {
            Ok(())
        }
    }

    enum Plan {
        Queue(FakeQueued),
        Uncertain,
    }

    /// Records every effect the controller asks of the core.
    #[derive(Default)]
    struct FakeBackend {
        selections: Mutex<Vec<FakeSelection>>,
        encrypted: AtomicBool,
        catalogs: AtomicU64,
        fetches: AtomicUsize,
        sends: AtomicUsize,
        plans: Mutex<VecDeque<Plan>>,
        /// The newest queued sticker the next lookup finds.
        pending: Mutex<Option<Pending<FakeQueued>>>,
        /// Requests an exact lookup finds by transaction; others are gone.
        exact: Mutex<Vec<Pending<FakeQueued>>>,
        lookups: AtomicUsize,
    }

    impl FakeBackend {
        fn plan(&self, plan: Plan) {
            self.plans.lock().unwrap().push_back(plan);
        }

        fn newest(&self, pending: Pending<FakeQueued>) {
            *self.pending.lock().unwrap() = Some(pending);
        }

        fn keep(&self, pending: Pending<FakeQueued>) {
            self.exact.lock().unwrap().push(pending);
        }
    }

    impl StickerBackend for FakeBackend {
        type Selection = FakeSelection;
        type Catalog = FakeCatalog;
        type Queued = FakeQueued;

        fn selection(&self, account: &str) -> Option<FakeSelection> {
            let selections = self.selections.lock().unwrap();
            selections
                .iter()
                .find(|selection| selection.is_current() && &*selection.account == account)
                .cloned()
        }

        async fn catalog(
            &self,
            selection: &FakeSelection,
            _room: &str,
            _thread: Option<&str>,
        ) -> Result<FakeCatalog, MatrixCoreError> {
            if !selection.is_current() {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            Ok(FakeCatalog {
                id: next_id(&self.catalogs),
                encrypted: self.encrypted.load(Ordering::SeqCst),
                entries: vec![
                    StickerEntry {
                        label: "Waving fox".into(),
                        shortcode: "fox_wave".into(),
                        pack: 0,
                    },
                    StickerEntry {
                        label: "Sleeping owl".into(),
                        shortcode: "owl_nap".into(),
                        pack: 0,
                    },
                ],
                packs: vec![StickerPack {
                    name: "Forest friends".into(),
                    attribution: None,
                    source: StickerPackSource::Room,
                    stickers: 2,
                }],
            })
        }

        async fn refresh(&self, _: &FakeCatalog) -> Result<Option<FakeCatalog>, MatrixCoreError> {
            Ok(None)
        }

        async fn preview(
            &self,
            _: &FakeCatalog,
            _: usize,
            _: Option<u64>,
        ) -> Result<Vec<u8>, MatrixCoreError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Err(MatrixCoreError::StickerUnavailable)
        }

        async fn send(
            &self,
            _: &FakeCatalog,
            _: usize,
            _: Option<u64>,
        ) -> Result<FakeQueued, MatrixCoreError> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            let plan = self.plans.lock().unwrap().pop_front();
            match plan {
                Some(Plan::Queue(queued)) => Ok(queued),
                Some(Plan::Uncertain) => Err(MatrixCoreError::ComposedSendUncertain),
                None => Err(MatrixCoreError::StickerUnavailable),
            }
        }

        async fn pending(
            &self,
            _: &FakeSelection,
            _: &str,
            _: Option<&str>,
        ) -> Result<Option<Pending<FakeQueued>>, MatrixCoreError> {
            Ok(self.pending.lock().unwrap().take())
        }

        async fn queued(
            &self,
            _: &FakeSelection,
            _: &str,
            transaction: &str,
        ) -> Result<Option<Pending<FakeQueued>>, MatrixCoreError> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let mut exact = self.exact.lock().unwrap();
            let index = exact
                .iter()
                .position(|pending| pending.queued.transaction == transaction);
            Ok(index.map(|index| exact.remove(index)))
        }

        async fn room_updates(&self, _: &str) -> Option<broadcast::Receiver<RoomUpdates>> {
            None
        }
    }

    fn queued(
        transaction: &str,
        aborts: &Arc<AtomicUsize>,
    ) -> (mpsc::UnboundedSender<QueuedSendUpdate>, FakeQueued) {
        let (updates, receiver) = mpsc::unbounded_channel();
        let queued = FakeQueued {
            transaction: transaction.into(),
            updates: receiver,
            aborts: aborts.clone(),
        };
        (updates, queued)
    }

    fn found(
        label: &str,
        others: usize,
        truncated: bool,
        queued: FakeQueued,
    ) -> Pending<FakeQueued> {
        Pending {
            label: label.into(),
            wedged: false,
            others,
            truncated,
            queued,
        }
    }

    async fn settle(ui: &AppWindow) {
        let (sender, receiver) = oneshot::channel();
        ui.as_weak()
            .upgrade_in_event_loop(move |_| {
                let _ = sender.send(());
            })
            .unwrap();
        receiver.await.unwrap();
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    }

    async fn until(ui: &AppWindow, what: &str, ready: impl Fn() -> bool) {
        for _ in 0..300 {
            settle(ui).await;
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("The sticker controller never reached: {what}");
    }

    fn visible_room(ui: &AppWindow, account: &str, room: &str) {
        ui.set_show_sign_in(false);
        ui.set_page(0);
        ui.set_live_session(true);
        ui.set_quick_switcher_open(false);
        ui.set_account_menu_open(false);
        ui.set_active_account_local_id(account.into());
        ui.set_active_room_id(room.into());
        ui.set_composer_mode(0);
        ui.set_composer_busy(false);
        ui.invoke_sticker_context_changed();
    }

    async fn open_ready(ui: &AppWindow, threaded: bool) {
        ui.invoke_open_stickers(threaded);
        until(ui, "a ready picker", || {
            ui.get_sticker_open() && ui.get_sticker_phase() == 1
        })
        .await;
    }

    fn first_key(ui: &AppWindow) -> SharedString {
        ui.get_sticker_items()
            .row_data(0)
            .expect("the picker shows a sticker")
            .key
    }

    pub(crate) async fn run(ui: &AppWindow, runtime: Handle) {
        let backend = Arc::new(FakeBackend::default());
        let first = FakeSelection::new("fixture-a");
        backend.selections.lock().unwrap().push(first.clone());
        let aborts = Arc::new(AtomicUsize::new(0));
        StickerUi::install(ui, backend.clone(), runtime.clone(), TEST_TIMING);
        let sends = || backend.sends.load(Ordering::SeqCst);

        // Admission: a command made in the same turn as a change of room,
        // account, composer mode, modal or session, before the context
        // handler runs, is refused before anything is fetched or queued. The
        // catalog needs consent, so no preview starts without an admitted one.
        backend.encrypted.store(true, Ordering::SeqCst);
        let changes: [(&str, fn(&AppWindow)); 5] = [
            ("room", |ui| {
                ui.set_active_room_id("!two:example.org".into())
            }),
            ("account", |ui| {
                ui.set_active_account_local_id("fixture-b".into())
            }),
            ("composer", |ui| ui.set_composer_mode(1)),
            ("modal", |ui| ui.set_quick_switcher_open(true)),
            ("session", |ui| ui.set_live_session(false)),
        ];
        for (context, change) in changes {
            visible_room(ui, "fixture-a", "!one:example.org");
            open_ready(ui, false).await;
            let key = first_key(ui);
            let fetches = backend.fetches.load(Ordering::SeqCst);
            change(ui);
            ui.invoke_sticker_consent(true);
            ui.invoke_sticker_highlight(key.clone());
            ui.invoke_sticker_navigate(1);
            ui.invoke_sticker_query("owl".into());
            ui.invoke_sticker_send(key);
            settle(ui).await;
            assert_eq!(sends(), 0, "{context}: a stale send was queued");
            let fetched = backend.fetches.load(Ordering::SeqCst);
            assert_eq!(fetched, fetches, "{context}: a stale preview was fetched");
            assert!(
                !ui.get_sticker_open(),
                "{context}: the stale picker stayed open"
            );
        }
        // The same holds for a thread picker whose root changes.
        visible_room(ui, "fixture-a", "!one:example.org");
        ui.set_thread_root_id("$root:example.org".into());
        ui.invoke_show_thread_panel();
        open_ready(ui, true).await;
        let key = first_key(ui);
        ui.set_thread_root_id("$other:example.org".into());
        ui.invoke_sticker_send(key);
        settle(ui).await;
        assert_eq!(sends(), 0, "thread: a stale send was queued");
        assert!(!ui.get_sticker_open());
        ui.set_thread_root_id("".into());
        visible_room(ui, "fixture-a", "!one:example.org");
        // Positive control: unchanged, the same consent is admitted and the
        // preview is fetched, so the zero counts above are meaningful.
        open_ready(ui, false).await;
        ui.invoke_sticker_consent(true);
        until(ui, "an admitted consent fetches a preview", || {
            backend.fetches.load(Ordering::SeqCst) > 0
        })
        .await;
        ui.invoke_sticker_dismiss();
        backend.encrypted.store(false, Ordering::SeqCst);

        // An accepted send persists: a context change hides its status but
        // never cancels it, and one tracked sticker blocks the next.
        let (updates, first_send) = queued("txn-one", &aborts);
        backend.plan(Plan::Queue(first_send));
        open_ready(ui, false).await;
        ui.invoke_sticker_send(first_key(ui));
        until(ui, "the queued sticker is observed", || {
            ui.get_sticker_send_visible()
        })
        .await;
        assert_eq!(sends(), 1);
        until(ui, "the spent choice closed the picker", || {
            !ui.get_sticker_open()
        })
        .await;
        assert!(ui.get_sticker_send_can_cancel());
        ui.set_active_room_id("!two:example.org".into());
        ui.invoke_sticker_context_changed();
        settle(ui).await;
        assert!(!ui.get_sticker_send_visible());
        visible_room(ui, "fixture-a", "!one:example.org");
        settle(ui).await;
        assert!(
            ui.get_sticker_send_visible(),
            "the status returns with its room"
        );
        let cancelled = aborts.load(Ordering::SeqCst);
        assert_eq!(cancelled, 0, "a context change cancelled it");
        open_ready(ui, false).await;
        assert!(!ui.get_sticker_can_send());
        ui.invoke_sticker_send(first_key(ui));
        settle(ui).await;
        assert_eq!(sends(), 1, "one tracked sticker at a time");
        ui.invoke_sticker_dismiss();
        // Cancel is single-flight: a second press before the result is refused.
        ui.invoke_sticker_send_cancel();
        ui.invoke_sticker_send_cancel();
        until(ui, "the single cancel reached the queue", || {
            aborts.load(Ordering::SeqCst) == 1
        })
        .await;
        settle(ui).await;
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
        updates.send(QueuedSendUpdate::Cancelled).unwrap();
        until(ui, "the cancellation is reported", || {
            ui.get_sticker_send_status() == "Sticker cancelled."
        })
        .await;
        ui.invoke_sticker_send_dismiss();
        settle(ui).await;
        assert!(!ui.get_sticker_send_visible());

        // An enqueue that did not return is held: the choice is spent, nothing
        // is retried or sent here, and no queued sticker lifts the hold, since
        // none can be shown to be the request that was being queued.
        backend.plan(Plan::Uncertain);
        open_ready(ui, false).await;
        ui.invoke_sticker_send(first_key(ui));
        until(ui, "the uncertain send is held", || {
            ui.get_sticker_send_uncertain()
        })
        .await;
        until(ui, "the uncertain choice closed the picker", || {
            !ui.get_sticker_open()
        })
        .await;
        assert_eq!(ui.get_sticker_send_status().as_str(), UNCERTAIN_STATUS);
        assert!(!ui.get_sticker_send_can_retry());
        assert!(!ui.get_sticker_send_can_cancel());
        // An older, unrelated queued sticker is observed beside the hold. It
        // is sent, and the hold remains.
        let (older_updates, older) = queued("txn-older", &aborts);
        backend.newest(found("Sleeping owl", 0, false, older));
        open_ready(ui, false).await;
        until(ui, "the older sticker is observed beside the hold", || {
            ui.get_sticker_send_can_cancel()
        })
        .await;
        assert!(ui.get_sticker_send_uncertain());
        assert!(!ui.get_sticker_can_send());
        ui.invoke_sticker_send(first_key(ui));
        settle(ui).await;
        assert_eq!(sends(), 2, "nothing new while a send is held");
        ui.invoke_sticker_dismiss();
        older_updates.send(QueuedSendUpdate::Sent).unwrap();
        until(ui, "the older sticker is sent", || {
            ui.get_sticker_send_status().ends_with("Sticker sent.")
        })
        .await;
        assert!(
            ui.get_sticker_send_uncertain(),
            "an unrelated sent sticker lifted the hold"
        );
        until(ui, "the sent status clears beside the hold", || {
            ui.get_sticker_send_status().as_str() == UNCERTAIN_STATUS
        })
        .await;
        // A second unrelated sticker is cancelled; the hold still remains.
        let (cancelled_updates, cancelled) = queued("txn-cancelled", &aborts);
        backend.newest(found("Waving fox", 0, false, cancelled));
        open_ready(ui, false).await;
        until(ui, "the second older sticker is observed", || {
            ui.get_sticker_send_can_cancel()
        })
        .await;
        ui.invoke_sticker_dismiss();
        cancelled_updates.send(QueuedSendUpdate::Cancelled).unwrap();
        until(ui, "the older sticker is cancelled", || {
            ui.get_sticker_send_status().ends_with("Sticker cancelled.")
        })
        .await;
        assert!(
            ui.get_sticker_send_uncertain(),
            "an unrelated cancelled sticker lifted the hold"
        );
        // A lookup over more queued stickers than it reads says so, and its
        // newest sticker is still not taken for the held send.
        let (newest_updates, newest) = queued("txn-newest", &aborts);
        backend.newest(found("Waving fox", 255, true, newest));
        open_ready(ui, false).await;
        until(ui, "the truncated lookup is reported", || {
            ui.get_sticker_send_status().contains("At least 255 more")
        })
        .await;
        assert!(ui.get_sticker_send_uncertain());
        assert!(!ui.get_sticker_can_send());
        ui.invoke_sticker_dismiss();
        // A confirmation applies only to the hold that was shown, where it is
        // still shown.
        ui.set_active_room_id("!two:example.org".into());
        ui.invoke_sticker_send_dismiss();
        visible_room(ui, "fixture-a", "!one:example.org");
        settle(ui).await;
        assert!(
            ui.get_sticker_send_uncertain(),
            "a hold was lifted from another room"
        );
        newest_updates.send(QueuedSendUpdate::Sent).unwrap();
        until(ui, "the newest sticker is sent and cleared", || {
            ui.get_sticker_send_status().as_str() == UNCERTAIN_STATUS
        })
        .await;
        // Only the user's confirmation lifts the hold.
        ui.invoke_sticker_send_dismiss();
        settle(ui).await;
        assert!(!ui.get_sticker_send_visible());
        open_ready(ui, false).await;
        assert!(
            ui.get_sticker_can_send(),
            "confirmed: a new sticker may be sent"
        );
        ui.invoke_sticker_dismiss();
        assert_eq!(sends(), 2);

        // A lost outcome is held with its transaction, never reported as
        // finished. Gone from the queue, the request was sent or cancelled:
        // the hold stays until the user confirms and nothing is sent again.
        let (lost_updates, lost) = queued("txn-lost", &aborts);
        backend.plan(Plan::Queue(lost));
        open_ready(ui, false).await;
        ui.invoke_sticker_send(first_key(ui));
        until(ui, "the sticker is observed", || {
            ui.get_sticker_send_can_cancel()
        })
        .await;
        assert_eq!(sends(), 3);
        drop(lost_updates);
        until(ui, "the lost request is gone from the queue", || {
            ui.get_sticker_send_status().as_str() == GONE_STATUS
        })
        .await;
        assert!(ui.get_sticker_send_uncertain());
        assert!(!ui.get_sticker_send_can_cancel());
        assert_eq!(backend.lookups.load(Ordering::SeqCst), 1);
        // An unrelated queued sticker is observed and sent beside it; the
        // hold stays, and a request known to be gone is not looked up again.
        let (other_updates, other) = queued("txn-unrelated", &aborts);
        backend.newest(found("Sleeping owl", 0, false, other));
        open_ready(ui, false).await;
        until(ui, "the unrelated sticker is observed", || {
            ui.get_sticker_send_can_cancel()
        })
        .await;
        ui.invoke_sticker_dismiss();
        other_updates.send(QueuedSendUpdate::Sent).unwrap();
        until(ui, "the unrelated sticker is sent and cleared", || {
            ui.get_sticker_send_status().as_str() == GONE_STATUS
        })
        .await;
        assert_eq!(backend.lookups.load(Ordering::SeqCst), 1);
        ui.invoke_sticker_send_dismiss();
        settle(ui).await;
        assert!(!ui.get_sticker_send_visible());
        assert_eq!(sends(), 3, "a lost outcome is never sent again");

        // Found still queued by its exact transaction, the request is observed
        // again in place and only then is its hold lifted.
        let (kept_updates, kept) = queued("txn-kept", &aborts);
        backend.plan(Plan::Queue(kept));
        open_ready(ui, false).await;
        ui.invoke_sticker_send(first_key(ui));
        until(ui, "the kept sticker is observed", || {
            ui.get_sticker_send_can_cancel()
        })
        .await;
        let (again_updates, kept_again) = queued("txn-kept", &aborts);
        backend.keep(found("Waving fox", 0, false, kept_again));
        drop(kept_updates);
        until(ui, "the exact request is observed again", || {
            ui.get_sticker_send_status().contains("still queued")
                && !ui.get_sticker_send_uncertain()
        })
        .await;
        assert!(ui.get_sticker_send_can_cancel());
        assert_eq!(backend.lookups.load(Ordering::SeqCst), 2);
        assert_eq!(sends(), 4, "observed again, never sent again");
        again_updates.send(QueuedSendUpdate::Sent).unwrap();
        until(ui, "the kept sticker is sent and cleared", || {
            !ui.get_sticker_send_visible()
        })
        .await;

        // After a restart a fresh controller finds the sticker still queued
        // here and observes it in place; nothing is sent again.
        let restarted = Arc::new(FakeBackend::default());
        let again = FakeSelection::new("fixture-a");
        restarted.selections.lock().unwrap().push(again.clone());
        let (_restart_updates, earlier) = queued("txn-earlier", &aborts);
        *restarted.pending.lock().unwrap() = Some(Pending {
            label: "Sleeping owl".into(),
            wedged: true,
            others: 1,
            truncated: false,
            queued: earlier,
        });
        first.revoke();
        StickerUi::install(ui, restarted.clone(), runtime, TEST_TIMING);
        visible_room(ui, "fixture-a", "!one:example.org");
        open_ready(ui, false).await;
        until(ui, "the earlier sticker is found", || {
            ui.get_sticker_send_visible()
        })
        .await;
        let status = ui.get_sticker_send_status();
        assert!(status.contains("still queued"));
        assert!(status.contains("1 more"));
        assert!(
            ui.get_sticker_send_can_retry(),
            "a wedged request is retried in place"
        );
        assert!(
            !ui.get_sticker_can_send(),
            "nothing new while it is observed"
        );
        assert_eq!(restarted.sends.load(Ordering::SeqCst), 0);

        // Ending the selection ends the picker, and the observed request
        // loses its outcome: it is held with its transaction, and the hold
        // outlives the selection. The request itself stays in the SDK queue.
        again.revoke();
        ui.invoke_sticker_context_changed();
        settle(ui).await;
        assert!(!ui.get_sticker_open());
        until(ui, "the revoked observation is held", || {
            ui.get_sticker_send_uncertain()
        })
        .await;
        assert_eq!(ui.get_sticker_send_status().as_str(), LOST_STATUS);
        assert!(!ui.get_sticker_send_can_retry());
        let cancelled = aborts.load(Ordering::SeqCst);
        assert_eq!(cancelled, 1, "revocation never cancels a queued request");
        // Back on the account, opening the picker looks the exact request up
        // again and observes it in place; nothing is sent again.
        let back = FakeSelection::new("fixture-a");
        restarted.selections.lock().unwrap().push(back.clone());
        let (back_updates, still) = queued("txn-earlier", &aborts);
        restarted.keep(found("Sleeping owl", 0, false, still));
        open_ready(ui, false).await;
        until(ui, "the exact request is observed again", || {
            !ui.get_sticker_send_uncertain() && ui.get_sticker_send_can_cancel()
        })
        .await;
        assert_eq!(restarted.lookups.load(Ordering::SeqCst), 1);
        assert_eq!(restarted.sends.load(Ordering::SeqCst), 0);
        ui.invoke_sticker_dismiss();
        back_updates.send(QueuedSendUpdate::Sent).unwrap();
        until(ui, "the earlier sticker is sent and cleared", || {
            !ui.get_sticker_send_visible()
        })
        .await;
        back.revoke();

        // Leave the window as the following scenarios expect it.
        ui.set_active_account_local_id("".into());
        ui.set_active_room_id("".into());
        ui.invoke_sticker_context_changed();
        settle(ui).await;
    }
}
