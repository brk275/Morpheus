//! Account-scoped desktop notifications from the Matrix SDK's push-rule feed.
//!
//! This module has no process-independent helper or scheduled delivery. The
//! queue and all handlers live only for the lifetime of Morpheus.
//!
//! A shown banner is remembered only as a bounded record of identifiers (an
//! opaque token, the account, room, event and thread IDs) so a click can open
//! it and reading the room can withdraw it. No message text is kept after the
//! banner is handed to the OS, and nothing is persisted.

mod native;
pub(crate) mod owned;
mod presenter;

#[cfg(test)]
use std::future::Future;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use matrix_sdk::{
    Room, deserialized_responses::RawAnySyncOrStrippedTimelineEvent,
    sync::Notification as MatrixNotification,
};
use morpheus_config::Config;
use morpheus_matrix::{
    AccountInfo, AccountManager, AccountSelection, MemberPrivacyError, NotificationMode,
};
use serde_json::Value;
use slint::{ComponentHandle, Weak as UiWeak};
use tokio::{
    runtime::Handle,
    sync::{Notify, mpsc, oneshot, watch},
};

// The presenter's report and withdrawal handle, for the one other surface
// that shows banners through it (incoming calls, see `show_silent_banner`).
#[cfg(feature = "native-calls")]
pub(crate) use self::presenter::{Guarded, Handover};
pub(crate) use self::presenter::{PresenterEvent, Withdrawal};
use crate::{AppWindow, navigation_target::NavigationTarget};
pub(crate) use native::{finish_native_work, observe_retained_native_work, shutdown_native};
use owned::{Close, NotificationWork, Role};
pub(crate) use owned::{NotificationDrainFailure, combine_app_work, observe_retained_app_work};

const MAX_NOTIFICATION_AGE: Duration = Duration::from_secs(5 * 60);
const SEEN_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_TEXT_CHARS: usize = 180;
const QUEUE_CAPACITY: usize = 64;
const UI_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Shown banners remembered for activation and withdrawal.
const MAX_SHOWN: usize = 32;
/// A banner clicked later than this opens nothing.
const SHOWN_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// A room rule changed in this window decides delivery until sync has had
/// time to bring the server's rules back to the SDK.
const ROOM_MODE_TTL: Duration = Duration::from_secs(2 * 60);
/// Clicked banners waiting for the UI; a newer click supersedes older ones.
const ACTIVATION_CAPACITY: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
struct VisibleRoom {
    account_id: String,
    room_id: String,
}

/// What the account's ignore list allows for one banner's sender, read with
/// the strict reader (`m.ignored_user_list` as this device's store has it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SenderIgnore {
    /// Read, and the sender is not on it.
    Allowed,
    /// Read, and the sender is on it (or is no valid user ID): no banner.
    Dropped,
    /// The list could not be read: notifications are paused for the
    /// account, failing closed, until a later read succeeds.
    Paused,
    /// The read was for another account selection: no banner.
    Stale,
}

/// `ignored` is whether the sender is on the list, or why the list was not
/// read. Only an authoritative read (`Ok`) allows a banner.
fn sender_ignore(ignored: Result<bool, MemberPrivacyError>) -> SenderIgnore {
    match ignored {
        Ok(false) => SenderIgnore::Allowed,
        Ok(true) => SenderIgnore::Dropped,
        Err(MemberPrivacyError::AccountChanged) => SenderIgnore::Stale,
        Err(_) => SenderIgnore::Paused,
    }
}

/// Accounts whose ignore list could not be read: their banners are paused.
/// Each change of an account's state gets a new number, so a window post can
/// tell whether a later change superseded it.
#[derive(Debug, Default)]
struct IgnorePause {
    paused: HashSet<String>,
    versions: HashMap<String, u64>,
    next: u64,
}

impl IgnorePause {
    /// Record one read; returns the account's new paused state when it
    /// changed. Only an unreadable list pauses, and only a successful read
    /// (whatever it says about the sender) resumes; a stale read is no
    /// evidence either way.
    fn observe(&mut self, account_id: &str, gate: SenderIgnore) -> Option<bool> {
        let changed = match gate {
            SenderIgnore::Paused => self.paused.insert(account_id.to_owned()).then_some(true),
            SenderIgnore::Allowed | SenderIgnore::Dropped => {
                self.paused.remove(account_id).then_some(false)
            }
            SenderIgnore::Stale => None,
        };
        if changed.is_some() {
            self.next = self.next.wrapping_add(1);
            self.versions.insert(account_id.to_owned(), self.next);
        }
        changed
    }

    fn paused(&self, account_id: &str) -> bool {
        self.paused.contains(account_id)
    }

    /// The number of `account_id`'s latest change; 0 before any.
    fn version(&self, account_id: &str) -> u64 {
        self.versions.get(account_id).copied().unwrap_or(0)
    }
}

/// What a window post for `account_id`'s change number `version` shows: the
/// pause state as it is when the post runs, never the one the change saw, and
/// nothing once a later change superseded it or another account is `shown`.
fn shown_pause(pause: &IgnorePause, account_id: &str, version: u64, shown: &str) -> Option<bool> {
    let latest = shown == account_id && pause.version(account_id) == version;
    latest.then(|| pause.paused(account_id))
}

/// The body of the window post for `account_id`'s change number `version`:
/// given the account on screen when the post runs, what the paused line
/// shows then (see `shown_pause`).
fn line_post(
    pause: Arc<Mutex<IgnorePause>>,
    account_id: String,
    version: u64,
) -> impl FnOnce(&str) -> Option<bool> + Send + 'static {
    move |shown: &str| {
        let state = pause.lock().unwrap_or_else(|error| error.into_inner());
        shown_pause(&state, &account_id, version, shown)
    }
}

#[derive(Clone, Debug)]
struct UiSnapshot {
    account_id: String,
    live: bool,
    visible_room: Option<VisibleRoom>,
    /// Sign-in, the app password, Manage accounts or a recovery key is on
    /// screen: banners then name only their account.
    private: bool,
}

/// While account access or a security flow is on screen, a banner names only
/// its account: no room, sender or message text, whatever the settings say.
fn privacy_ceiling(config: &mut Config, private: bool) {
    if private {
        config.notifications.show_room_and_sender = false;
        config.notifications.show_message_preview = false;
    }
}

/// Sign-in, the app password, Manage accounts or a recovery key is on screen
/// (see `UiSnapshot::private`). UI thread only.
pub(crate) fn private_screen(ui: &AppWindow) -> bool {
    ui.get_show_sign_in()
        || ui.get_access_open()
        || ui.get_manage_accounts_open()
        || !ui.get_security_recovery_key().is_empty()
}

/// Whether a silent banner of another surface (an incoming call) may show
/// for `account_id`, and if so whether it may name more than the account:
/// `None` while notifications are off globally or for the account, never more
/// than the account on a private screen or with room and sender hidden.
#[cfg(feature = "native-calls")]
fn silent_identity(config: &Config, account_id: &str, private: bool) -> Option<bool> {
    let mut config = config.clone();
    privacy_ceiling(&mut config, private);
    config
        .notifications_enabled_for(account_id)
        .then_some(config.notifications.show_room_and_sender)
}

/// The text of an incoming call's banner: always its account, the caller's
/// name only with `identity`, and never a room, event or anything the call
/// says. Bounded plain text, made safe like this module's own banners.
#[cfg(feature = "native-calls")]
pub(crate) fn call_banner_text(
    account_user_id: &str,
    caller_user_id: &str,
    identity: bool,
) -> (String, String) {
    let account = safe_text(account_user_id, 64);
    let summary = format!("Incoming call · {account}");
    let body = if identity {
        let caller = safe_text(sender_localpart(caller_user_id), 48);
        format!("{caller} is calling")
    } else {
        "Open Morpheus to see the call".to_owned()
    };
    (summary, body)
}

/// A banner another surface hands to the presenter: bounded plain text,
/// and never a sound of its own.
#[cfg(feature = "native-calls")]
fn silent_plan(summary: &str, body: &str) -> PlannedNotification {
    PlannedNotification {
        summary: safe_text(summary, MAX_TEXT_CHARS),
        body: safe_text(body, MAX_TEXT_CHARS),
        sound: false,
    }
}

/// The OS took no banner. It carries no backend or server text.
#[cfg(feature = "native-calls")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BannerUnavailable;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Message,
    Invite,
}

#[derive(Clone, Debug)]
struct Candidate {
    account_id: String,
    account_user_id: String,
    room_id: String,
    room_name: String,
    sender_id: String,
    event_key: String,
    /// The Matrix event, for messages; an invitation has none to open.
    event_id: Option<String>,
    /// Root of the thread the message was sent in.
    thread_root: Option<String>,
    timestamp_ms: Option<u64>,
    kind: Kind,
    body: Option<String>,
    sdk_highlight: bool,
    sdk_sound: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlannedNotification {
    summary: String,
    body: String,
    sound: bool,
}

struct QueuedNotification {
    candidate: Candidate,
    selection: AccountSelection,
}

struct PendingNotification {
    queued: QueuedNotification,
    room: Room,
    // The fixture receipt field drops after the actual queued SDK Room clone.
    #[cfg(test)]
    _released: Option<QueueRelease>,
}

#[cfg(test)]
struct QueueRelease(Option<oneshot::Sender<()>>);
#[cfg(test)]
impl Drop for QueueRelease {
    fn drop(&mut self) {
        if let Some(send) = self.0.take() {
            let _ = send.send(());
        }
    }
}

/// Unlike a channel receiver parked behind an entered native show, the
/// account watcher can synchronously release revoked queued Room/Client owners.
/// No SDK operation runs under this lock. At most QUEUE_CAPACITY records live.
#[derive(Default)]
struct NotificationQueue {
    pending: Mutex<VecDeque<PendingNotification>>,
    wake: Notify,
}

impl NotificationQueue {
    fn push(&self, item: PendingNotification, close: &Close) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        pending.retain(|item| item.queued.selection.is_current());
        if !close.is_closed()
            && item.queued.selection.is_current()
            && pending.len() < QUEUE_CAPACITY
        {
            pending.push_back(item);
            self.wake.notify_one();
        }
    }

    fn discard_revoked(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|item| item.queued.selection.is_current());
    }

    fn clear(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    async fn next(&self, close: &Close) -> Option<PendingNotification> {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if close.is_closed() {
                return None;
            }
            if let Some(item) = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front()
            {
                return Some(item);
            }
            close.guard(notified).await?;
        }
    }
}

/// One banner handed to the OS: identifiers only.
struct ShownBanner {
    token: u64,
    target: NavigationTarget,
    account_user_id: String,
    shown_at: Instant,
    withdrawal: Option<Withdrawal>,
}

/// The bounded, in-memory record of shown banners, oldest first.
#[derive(Default)]
struct ShownBanners(VecDeque<ShownBanner>);

impl ShownBanners {
    /// Remember one banner. The oldest record is forgotten past the bound and
    /// its banner is withdrawn where possible, since a click on it can no
    /// longer be resolved.
    fn insert(&mut self, banner: ShownBanner) {
        while self.0.len() >= MAX_SHOWN {
            if let Some(oldest) = self.0.pop_front() {
                withdraw(oldest);
            }
        }
        self.0.push_back(banner);
    }

    /// Consume the record of a clicked banner. Expired records resolve to
    /// nothing.
    fn take(&mut self, token: u64, now: Instant) -> Option<ShownBanner> {
        let index = self.0.iter().position(|banner| banner.token == token)?;
        let banner = self.0.remove(index)?;
        (now.duration_since(banner.shown_at) < SHOWN_TTL).then_some(banner)
    }

    /// Withdraw and forget banners past `SHOWN_TTL`: a click on one opens
    /// nothing, and withdrawing it releases its listener.
    fn prune(&mut self, now: Instant) {
        let mut kept = VecDeque::with_capacity(self.0.len());
        for banner in self.0.drain(..) {
            if now.duration_since(banner.shown_at) < SHOWN_TTL {
                kept.push_back(banner);
            } else {
                withdraw(banner);
            }
        }
        self.0 = kept;
    }

    /// Withdraw and forget the oldest banner that can be withdrawn, so its
    /// listener is free for a new banner.
    fn withdraw_oldest(&mut self) {
        if let Some(index) = self.0.iter().position(|banner| banner.withdrawal.is_some())
            && let Some(banner) = self.0.remove(index)
        {
            withdraw(banner);
        }
    }

    /// Withdraw every banner of one room that can be withdrawn and forget it.
    /// Banners the platform cannot close stay clickable.
    fn withdraw_room(&mut self, account_id: &str, room_id: &str) {
        let mut kept = VecDeque::with_capacity(self.0.len());
        for banner in self.0.drain(..) {
            if banner.target.account_id() == account_id
                && banner.target.room_id() == room_id
                && banner.withdrawal.is_some()
            {
                withdraw(banner);
            } else {
                kept.push_back(banner);
            }
        }
        self.0 = kept;
    }

    /// Withdraw and forget the banners of accounts no longer saved, or of
    /// every account with `None`.
    fn retain_accounts(&mut self, saved: Option<&[AccountInfo]>) {
        let mut kept = VecDeque::with_capacity(self.0.len());
        for banner in self.0.drain(..) {
            let still_saved = saved.is_some_and(|saved| {
                saved.iter().any(|account| {
                    account.id == banner.target.account_id()
                        && account.user_id == banner.account_user_id
                })
            });
            if still_saved {
                kept.push_back(banner);
            } else {
                withdraw(banner);
            }
        }
        self.0 = kept;
    }

    /// Rooms of `account_id` with a banner Morpheus can still withdraw.
    fn withdrawable_rooms(&self, account_id: &str) -> Vec<String> {
        let mut rooms = Vec::new();
        for banner in &self.0 {
            if banner.withdrawal.is_some()
                && banner.target.account_id() == account_id
                && !rooms.iter().any(|room| room == banner.target.room_id())
            {
                rooms.push(banner.target.room_id().to_owned());
            }
        }
        rooms
    }
}

fn withdraw(banner: ShownBanner) {
    if let Some(withdrawal) = banner.withdrawal {
        withdrawal.withdraw();
    }
}

/// The UI supplies the latest valid config. The worker asks Slint whether a
/// room is on screen at delivery time, so a navigation change cannot leave a
/// stale room suppression cache.
/// This is the manager's single process-lifetime notification owner.
/// `watch_accounts` registers one Matrix notification callback for each loaded
/// SDK client, including a fresh client restored for a previously seen account.
/// Both the callback and the delivery worker re-check the active
/// account, so queued work from an earlier selection is discarded.
pub struct DesktopNotifications {
    manager: Arc<AccountManager>,
    config: RwLock<Config>,
    ui: UiWeak<AppWindow>,
    work: NotificationWork,
    native: native::NativeBook,
    closing: Close,
    queued: Arc<NotificationQueue>,
    shown: Mutex<ShownBanners>,
    next_token: AtomicU64,
    room_modes: Mutex<HashMap<(String, String), (NotificationMode, Instant)>>,
    /// Accounts whose ignore list could not be read (fail closed). Shared
    /// with the window's posts, which read it again when they run.
    ignore_pause: Arc<Mutex<IgnorePause>>,
    presenter_events: mpsc::Sender<PresenterEvent>,
    activation_tx: mpsc::Sender<NavigationTarget>,
    activations: Mutex<Option<mpsc::Receiver<NavigationTarget>>>,
}

impl DesktopNotifications {
    pub fn start(
        manager: Arc<AccountManager>,
        config: Config,
        ui: UiWeak<AppWindow>,
        runtime: &Handle,
    ) -> Arc<Self> {
        let work = NotificationWork::desktop();
        let closing = work.cancellation();
        let (presenter_events, presenter_receiver) = mpsc::channel(MAX_SHOWN);
        let (activation_tx, activations) = mpsc::channel(ACTIVATION_CAPACITY);
        let service = Arc::new(Self {
            manager: manager.clone(),
            config: RwLock::new(config),
            ui,
            work,
            native: native::NativeBook::desktop(),
            closing,
            queued: Arc::new(NotificationQueue::default()),
            shown: Mutex::new(ShownBanners::default()),
            next_token: AtomicU64::new(1),
            room_modes: Mutex::new(HashMap::new()),
            ignore_pause: Arc::new(Mutex::new(IgnorePause::default())),
            presenter_events,
            activation_tx,
            activations: Mutex::new(Some(activations)),
        });
        service.spawn_app_task(
            Role::Accounts,
            runtime,
            watch_accounts(
                manager,
                Arc::downgrade(&service),
                service.cancellation(),
                service.queued.clone(),
            ),
        );
        service.spawn_app_task(
            Role::Delivery,
            runtime,
            deliver_queued(
                Arc::downgrade(&service),
                service.queued.clone(),
                service.cancellation(),
            ),
        );
        service.spawn_app_task(
            Role::PresenterEvents,
            runtime,
            resolve_presenter_events(
                Arc::downgrade(&service),
                presenter_receiver,
                service.cancellation(),
            ),
        );
        service
    }

    /// Call only with a successfully loaded configuration. Invalid external
    /// TOML edits should retain the previous valid policy.
    pub fn apply_config(&self, config: &Config) {
        *self
            .config
            .write()
            .unwrap_or_else(|error| error.into_inner()) = config.clone();
    }

    /// Call before stopping Matrix sync and leaving the UI event loop. Queued
    /// banners are dropped and shown banners are withdrawn where the platform
    /// allows, since a quit app can no longer open them.
    pub fn shutdown(&self) {
        self.work.close();
        self.native.close();
        self.queued.clear();
        self.shown().retain_accounts(None);
    }

    /// Morpheus is quitting; no more notification work starts.
    pub(crate) fn is_stopped(&self) -> bool {
        self.closing.is_closed()
    }

    pub(crate) fn cancellation(&self) -> Close {
        self.closing.clone()
    }

    pub(crate) fn spawn_app_task(
        &self,
        role: Role,
        runtime: &Handle,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        self.work.spawn(role, runtime, future)
    }

    /// Join this service's actual application tasks, including cfg(test)
    /// local owners. Never infer their completion from empty production slots.
    /// Native presenter/listener completion is a separate boundary.
    pub(crate) async fn finish_app_work(&self) -> Result<(), NotificationDrainFailure> {
        self.shutdown();
        self.work.finish_app_work().await
    }

    /// Actual local native owner, including fixture-local constructors.
    pub(crate) async fn finish_native_work(&self) -> Result<(), NotificationDrainFailure> {
        self.native.finish().await
    }

    /// The single receiver of clicked banners' targets. Each target is
    /// already checked against the saved accounts; the caller still has to
    /// open it through the protected account switch.
    pub(crate) fn take_activations(&self) -> Option<mpsc::Receiver<NavigationTarget>> {
        self.activations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// Record a room rule the homeserver just accepted from this window, so
    /// delivery follows it before sync returns the updated push rules.
    pub(crate) fn note_room_mode(&self, account_id: &str, room_id: &str, mode: NotificationMode) {
        let mut modes = self
            .room_modes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        modes.retain(|_, (_, noted)| now.duration_since(*noted) < ROOM_MODE_TTL);
        modes.insert((account_id.to_owned(), room_id.to_owned()), (mode, now));
    }

    fn room_mode(&self, account_id: &str, room_id: &str) -> Option<NotificationMode> {
        self.room_modes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(account_id.to_owned(), room_id.to_owned()))
            .filter(|(_, noted)| noted.elapsed() < ROOM_MODE_TTL)
            .map(|(mode, _)| *mode)
    }

    /// Read the account's ignore list strictly and judge `sender_id` by it,
    /// recording whether the account's banners are paused. The reader takes
    /// the manager's state lock briefly; no lock is held here.
    async fn read_ignore(&self, selection: &AccountSelection, sender_id: &str) -> SenderIgnore {
        let read = self.manager.cached_ignored_users_selected(selection).await;
        // A sender that is no valid user ID is not allowed either.
        let sender = matrix_sdk::ruma::UserId::parse(sender_id).ok();
        let ignored = read.map(|users| sender.as_ref().is_none_or(|sender| users.contains(sender)));
        let gate = sender_ignore(ignored);
        self.record_ignore(selection.account_id(), gate);
        gate
    }

    /// Record an ignore-list read for `account_id` (also from Settings) and
    /// show a change of its paused state on screen.
    pub(crate) fn record_ignore_read(
        &self,
        account_id: &str,
        read: &Result<morpheus_matrix::IgnoredUsers, MemberPrivacyError>,
    ) {
        let gate = match read {
            Ok(_) => SenderIgnore::Allowed,
            Err(error) => sender_ignore(Err(*error)),
        };
        self.record_ignore(account_id, gate);
    }

    /// The delivery gate does not depend on what this shows: each banner
    /// judges its own strict read.
    fn record_ignore(&self, account_id: &str, gate: SenderIgnore) {
        let version = {
            let mut pause = self
                .ignore_pause
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            pause
                .observe(account_id, gate)
                .map(|_| pause.version(account_id))
        };
        let Some(version) = version else {
            return;
        };
        // Posts of different reads can reach the window out of order. Only
        // the post of an account's latest change shows anything, and it shows
        // the state as it is when it runs (see `shown_pause`).
        let post = line_post(self.ignore_pause.clone(), account_id.to_owned(), version);
        let closing = self.cancellation();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            if closing.is_closed() || ui.get_application_closing() {
                return;
            }
            let shown = ui.get_active_account_local_id();
            if let Some(paused) = post(shown.as_str()) {
                ui.global::<crate::NotificationRulesState>()
                    .set_ignore_paused(paused);
            }
        });
    }

    /// Whether `account_id`'s banners are paused because its ignore list
    /// could not be read.
    pub(crate) fn ignore_paused(&self, account_id: &str) -> bool {
        self.ignore_pause
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .paused(account_id)
    }

    /// The room was read: withdraw its banners where the platform allows.
    pub(crate) fn withdraw_room(&self, account_id: &str, room_id: &str) {
        self.shown().withdraw_room(account_id, room_id);
    }

    /// Rooms of `account_id` with banners that reading would withdraw.
    pub(crate) fn withdrawable_rooms(&self, account_id: &str) -> Vec<String> {
        self.shown().withdrawable_rooms(account_id)
    }

    /// For a silent banner of another surface (an incoming call) on
    /// `account_id`: `None` while Morpheus is quitting or notifications are
    /// off globally or for the account, else whether it may name more than
    /// the account (`private`: a private screen is up). Read from the latest
    /// valid configuration each time.
    #[cfg(feature = "native-calls")]
    pub(crate) fn silent_banner_identity(&self, account_id: &str, private: bool) -> Option<bool> {
        if self.is_stopped() {
            return None;
        }
        let config = self
            .config
            .read()
            .unwrap_or_else(|error| error.into_inner());
        silent_identity(&config, account_id, private)
    }

    /// Hand one silent banner of another surface (an incoming call) to the
    /// OS through this module's presenter: the same pinned backends,
    /// application identity, bounded listener slots and Linux withdrawal.
    /// Nothing of it is recorded here; its listener reports `token` on
    /// `events`, the caller's own channel and token space. `handover`, and
    /// Morpheus not quitting, are checked right before the backend's actual
    /// send (`presenter::Handover`); refused, nothing reaches the OS. With
    /// every listener taken, this module's oldest withdrawable banner is
    /// withdrawn first, as for its own. The awaited result is only the real
    /// submission result; the fixed native book owns the worker/listener joins.
    #[cfg(feature = "native-calls")]
    pub(crate) async fn show_silent_banner(
        self: &Arc<Self>,
        summary: &str,
        body: &str,
        token: u64,
        events: mpsc::Sender<PresenterEvent>,
        runtime: Handle,
        handover: Handover,
    ) -> Result<Guarded, BannerUnavailable> {
        if self.is_stopped() {
            return Ok(Guarded::Refused);
        }
        let full = self.native.listeners_full();
        {
            let mut shown = self.shown();
            shown.prune(Instant::now());
            if full {
                shown.withdraw_oldest();
            }
        }
        // Quitting is part of the handover, wherever the send happens.
        let service = Arc::downgrade(self);
        let checked: Handover = Arc::new(move || {
            service
                .upgrade()
                .is_some_and(|service| !service.is_stopped())
                && handover()
        });
        // The backend's error text is dropped here: it reaches no status or log.
        let shown = presenter::show_guarded(
            &self.native,
            silent_plan(summary, body),
            token,
            events,
            runtime,
            checked,
        )
        .await
        .map_err(|_| BannerUnavailable)?;
        match shown {
            // Quitting meanwhile: withdrawn where the platform allows.
            Guarded::Handed(Some(withdrawal)) if self.is_stopped() => {
                withdrawal.withdraw();
                Ok(Guarded::Handed(None))
            }
            shown => Ok(shown),
        }
    }

    fn shown(&self) -> std::sync::MutexGuard<'_, ShownBanners> {
        self.shown.lock().unwrap_or_else(|error| error.into_inner())
    }

    async fn ui_snapshot(&self) -> Option<UiSnapshot> {
        let (send, receive) = oneshot::channel();
        let closing = self.cancellation();
        if closing.is_closed() {
            return None;
        }
        self.ui
            .upgrade_in_event_loop(move |ui| {
                if closing.is_closed() || ui.get_application_closing() {
                    return;
                }
                let account_id = ui.get_active_account_local_id().to_string();
                let room_id = ui.get_active_room_id().to_string();
                let live = ui.get_live_session();
                // A room left open in a window the user is not using still
                // notifies; only the active window suppresses its banners.
                let visible_room = if live
                    && ui.get_page() == 0
                    && ui.window().is_visible()
                    && !ui.window().is_minimized()
                    && crate::notification_center::window_active(&ui)
                    && !account_id.is_empty()
                    && !room_id.is_empty()
                {
                    Some(VisibleRoom {
                        account_id: account_id.clone(),
                        room_id,
                    })
                } else {
                    None
                };
                let private = private_screen(&ui);
                let _ = send.send(UiSnapshot {
                    account_id,
                    live,
                    visible_room,
                    private,
                });
            })
            .ok()?;
        tokio::time::timeout(UI_QUERY_TIMEOUT, receive)
            .await
            .ok()?
            .ok()
    }

    /// SDK callbacks capture the exact selection synchronously and only
    /// enqueue bounded data. No Room/store await is left inside SDK sync.
    fn enqueue(
        &self,
        account: &AccountInfo,
        selection: AccountSelection,
        notification: MatrixNotification,
        room: Room,
    ) {
        if self.is_stopped() || !selection.is_current() || selection.account_id() != account.id {
            return;
        }
        let Some(candidate) =
            Candidate::from_matrix(account, room.room_id().as_str(), notification)
        else {
            return;
        };
        self.queued.push(
            PendingNotification {
                queued: QueuedNotification {
                    candidate,
                    selection,
                },
                room,
                #[cfg(test)]
                _released: None,
            },
            &self.closing,
        );
    }

    /// Read the account, UI visibility, and current policy together as late as
    /// possible. A queued event must never carry an earlier account's preview
    /// setting into the native notification presenter.
    async fn plan_current(&self, queued: &QueuedNotification) -> Option<PlannedNotification> {
        let candidate = &queued.candidate;
        if self.is_stopped()
            || !queued.selection.is_current()
            || queued.selection.account_id() != candidate.account_id
        {
            return None;
        }
        // Nothing is looked up for an account no longer live on screen.
        let ui = self
            .closing
            .guard(queued.selection.guard(self.ui_snapshot()))
            .await
            .flatten()
            .flatten()?;
        if !ui.live || ui.account_id != candidate.account_id {
            return None;
        }
        // A room mode accepted in this window, read again after every await.
        let noted = || self.room_mode(&candidate.account_id, &candidate.room_id);
        let lookup = async {
            self.manager
                .room_notification_rule_selected(&queued.selection, &candidate.room_id)
                .await
                .ok()
                .map(|rule| rule.effective)
        };
        // The ignore list is read strictly here, at both preflights (before
        // queuing and at final delivery): unreadable fails closed. It is read
        // before the account and the window, never after them.
        let snapshot = move || {
            final_snapshot(
                self.read_ignore(&queued.selection, &candidate.sender_id),
                self.manager.active_account(),
                self.ui_snapshot(),
            )
        };
        let config = || {
            self.config
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        };
        // Account changes can happen during any await. Returning to the same
        // account must not revive this earlier notification.
        let current = || !self.is_stopped() && queued.selection.is_current();
        self.closing
            .guard(queued.selection.guard(plan_after_lookup(
                candidate, noted, lookup, snapshot, config, current, now_ms,
            )))
            .await
            .flatten()
            .flatten()
    }

    /// Resolve one clicked banner to its target. Only a remembered, unexpired
    /// banner of a still-saved account with the same Matrix user resolves; a
    /// removed account is never brought back by an old banner.
    async fn resolve_activation(&self, token: u64) {
        if self.is_stopped() {
            return;
        }
        let Some(banner) = self.shown().take(token, Instant::now()) else {
            return;
        };
        let Some(accounts) = self.closing.guard(self.manager.accounts()).await else {
            return;
        };
        let Some(status) = self.closing.guard(self.manager.app_password_status()).await else {
            return;
        };
        let pending_removal = status
            .pending_removals
            .iter()
            .any(|id| id == banner.target.account_id());
        let saved = accounts.iter().any(|account| {
            account.id == banner.target.account_id() && account.user_id == banner.account_user_id
        });
        if !saved || pending_removal || self.is_stopped() {
            withdraw(banner);
            return;
        }
        // Bounded: identifiers only, and a full queue drops this click.
        let _ = self.activation_tx.try_send(banner.target.clone());
    }
}

impl Drop for DesktopNotifications {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn watch_accounts(
    manager: Arc<AccountManager>,
    service: Weak<DesktopNotifications>,
    closing: Close,
    queue: Arc<NotificationQueue>,
) {
    let mut selected = manager.subscribe_active_account();
    loop {
        if closing.is_closed() {
            return;
        }
        if let Some(service) = service.upgrade() {
            service.queued.discard_revoked();
        }
        let account = selected.borrow().clone();
        let selection = account
            .as_ref()
            .and_then(|account| manager.account_selection(&account.id));
        if let Some(account) = account {
            let weak = service.clone();
            let callback_account = account.clone();
            // The SDK invokes this closure before awaiting its returned future.
            // No old callback can reconstruct authority after returning here.
            let registration = manager.register_notification_handler_for(
                &account.id,
                move |notification, room, _client| {
                    if let Some(service) = weak.upgrade() {
                        if let Some(selection) =
                            service.manager.account_selection(&callback_account.id)
                        {
                            service.enqueue(&callback_account, selection, notification, room);
                        }
                    }
                    std::future::ready(())
                },
            );
            if closing.guard(registration).await.is_none() {
                return;
            }
        }
        if let Some(service) = service.upgrade() {
            service.queued.discard_revoked();
            let saved = match &selection {
                Some(selection) => closing
                    .guard(selection.guard(manager.accounts()))
                    .await
                    .flatten(),
                None => closing.guard(manager.accounts()).await,
            };
            if let Some(saved) = saved {
                if !closing.is_closed() {
                    service.shown().retain_accounts(Some(saved.as_slice()));
                }
            }
            service.queued.discard_revoked();
        }
        if service.upgrade().is_none() {
            return;
        }
        match wait_account_change(&queue, &mut selected, selection.as_ref(), &closing).await {
            None => return,
            Some(AccountChange::Published) => {}
            Some(AccountChange::Revoked) => {
                if closing
                    .guard(selected.changed())
                    .await
                    .is_none_or(|changed| changed.is_err())
                {
                    return;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccountChange {
    Published,
    Revoked,
}

async fn wait_account_change(
    queue: &NotificationQueue,
    selected: &mut watch::Receiver<Option<AccountInfo>>,
    selection: Option<&AccountSelection>,
    closing: &Close,
) -> Option<AccountChange> {
    let revoked = async {
        match selection {
            Some(selection) => selection.revoked().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        biased;
        _ = closing.cancelled() => None,
        _ = revoked => {
            // Release original queued client owners before waiting for core's
            // later active-feed publication/stop_sync completion.
            queue.discard_revoked();
            Some(AccountChange::Revoked)
        },
        changed = selected.changed() => changed.ok().map(|()| AccountChange::Published),
    }
}

/// Existing disposable-account fixture only, with its real original selection
/// and cache Room. No display or additional network request runs here.
#[cfg(test)]
pub(crate) async fn exercise_queue_revocation_for_test(
    manager: &AccountManager,
    selection: AccountSelection,
    room: Room,
) -> Result<(), &'static str> {
    let mut selected = manager.subscribe_active_account();
    let Some(account) = selected.borrow().clone() else {
        return Err("notification fixture has no active account");
    };
    if !selection.is_current() || selection.account_id() != account.id {
        return Err("notification fixture selection is not current");
    }
    let queue = NotificationQueue::default();
    let closing = Close::default();
    let (send, mut released) = oneshot::channel();
    let candidate = Candidate {
        account_id: account.id.clone(),
        account_user_id: account.user_id.clone(),
        room_id: room.room_id().to_string(),
        room_name: String::new(),
        sender_id: "@notification-fixture:example.org".into(),
        event_key: "notification-owner-probe".into(),
        event_id: None,
        thread_root: None,
        timestamp_ms: None,
        kind: Kind::Message,
        body: None,
        sdk_highlight: false,
        sdk_sound: false,
    };
    queue.push(
        PendingNotification {
            queued: QueuedNotification {
                candidate,
                selection: selection.clone(),
            },
            room,
            _released: Some(QueueRelease(Some(send))),
        },
        &closing,
    );
    let held_before = matches!(
        released.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    );
    let mut waiting = Box::pin(wait_account_change(
        &queue,
        &mut selected,
        Some(&selection),
        &closing,
    ));
    let first = std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx))).await;
    // Real synchronous authority revocation precedes active-feed publication;
    // no test edits or fabricates the selection capability.
    manager.close_selection_admission();
    let outcome = tokio::time::timeout(UI_QUERY_TIMEOUT, &mut waiting).await;
    drop(waiting);
    let released_before_cleanup = released.try_recv().is_ok();
    let publication_unchanged = matches!(selected.has_changed(), Ok(false))
        && selected
            .borrow()
            .as_ref()
            .is_some_and(|current| current.id == account.id);
    // Cleanup cannot repair the captured observations and precedes errors.
    queue.clear();
    if !held_before
        || !first.is_pending()
        || !matches!(outcome, Ok(Some(AccountChange::Revoked)))
        || !released_before_cleanup
        || !publication_unchanged
    {
        return Err(
            "notification queue did not release its revoked room before active publication",
        );
    }
    Ok(())
}

async fn deliver_queued(
    service: Weak<DesktopNotifications>,
    queue: Arc<NotificationQueue>,
    closing: Close,
) {
    let mut seen = HashMap::<String, Instant>::new();
    loop {
        let Some(PendingNotification {
            mut queued, room, ..
        }) = queue.next(&closing).await
        else {
            return;
        };
        let Some(service) = service.upgrade() else {
            return;
        };
        let name = closing
            .guard(queued.selection.guard(room.display_name()))
            .await;
        drop(room);
        let Some(Some(name)) = name else {
            continue;
        };
        queued.candidate.room_name = name
            .map(|name| name.to_string())
            .unwrap_or_else(|_| queued.candidate.room_id.clone());
        if service.plan_current(&queued).await.is_none() {
            continue;
        }
        let now = Instant::now();
        seen.retain(|_, observed| now.duration_since(*observed) < SEEN_TTL);
        if seen
            .insert(queued.candidate.event_key.clone(), now)
            .is_some()
        {
            continue;
        }
        let target = NavigationTarget::new(
            &queued.candidate.account_id,
            &queued.candidate.room_id,
            queued.candidate.event_id.as_deref(),
            queued.candidate.thread_root.as_deref(),
        );
        // Preparation has no live Room. The weak handover captures only this
        // ORIGINAL selection/candidate and never reconstructs same-ID authority.
        let Some(plan) = service.plan_current(&queued).await else {
            continue;
        };
        let full = service.native.listeners_full();
        {
            let mut shown = service.shown();
            shown.prune(Instant::now());
            if full {
                shown.withdraw_oldest();
            }
        }
        let token = service.next_token.fetch_add(1, Ordering::AcqRel);
        let candidate = queued.candidate.clone();
        let original = Arc::downgrade(&service);
        let runtime = Handle::current();
        let checking_runtime = runtime.clone();
        let expected = plan.clone();
        let handover: presenter::Handover = Arc::new(move || {
            let Some(service) = original.upgrade() else {
                return false;
            };
            if service.is_stopped() || !queued.selection.is_current() {
                return false;
            }
            // Revalidate all original policy/privacy/ignore/visibility facts
            // at native entry. A different plan is refused, never leaked under
            // a newer privacy ceiling. This future is original-selection guarded.
            checking_runtime
                .block_on(service.plan_current(&queued))
                .as_ref()
                == Some(&expected)
        });
        let shown = presenter::show(
            &service.native,
            plan,
            token,
            service.presenter_events.clone(),
            runtime,
            handover,
        )
        .await;
        match shown {
            Ok(presenter::Guarded::Refused) => (),
            Ok(presenter::Guarded::Handed(withdrawal)) => {
                if service.is_stopped() {
                    if let Some(withdrawal) = withdrawal {
                        withdrawal.withdraw();
                    }
                } else if let Some(target) = target {
                    service.withdraw_room(&candidate.account_id, &candidate.room_id);
                    let publication = withdrawal.as_ref().and_then(Withdrawal::publication);
                    service.shown().insert(ShownBanner {
                        token,
                        target,
                        account_user_id: candidate.account_user_id,
                        shown_at: Instant::now(),
                        withdrawal,
                    });
                    // Publish only AFTER the real shown record exists and its
                    // lock is released. An early exact response is delivered once.
                    if let Some(publication) = publication {
                        publication.publish();
                    }
                } else if let Some(withdrawal) = withdrawal {
                    withdrawal.withdraw();
                }
            }
            Err(()) => eprintln!("Could not display a desktop notification."),
        }
    }
}

async fn resolve_presenter_events(
    service: Weak<DesktopNotifications>,
    mut events: mpsc::Receiver<PresenterEvent>,
    closing: Close,
) {
    while let Some(Some(event)) = closing.guard(events.recv()).await {
        let Some(service) = service.upgrade() else {
            return;
        };
        match event {
            PresenterEvent::Activated(token) => service.resolve_activation(token).await,
            PresenterEvent::Closed(token) | PresenterEvent::Failed(token) => {
                let _ = service.shown().take(token, Instant::now());
            }
        }
    }
}

impl Candidate {
    fn from_matrix(
        account: &AccountInfo,
        room_id: &str,
        notification: MatrixNotification,
    ) -> Option<Self> {
        let sdk_highlight = notification
            .actions
            .iter()
            .any(|action| action.is_highlight());
        let sdk_sound = notification
            .actions
            .iter()
            .any(|action| action.sound().is_some());
        let (kind, sender_id, event_key, event_id, thread_root, timestamp_ms, body) =
            match notification.event {
                RawAnySyncOrStrippedTimelineEvent::Sync(raw) => {
                    let event_type: String = raw.get_field("type").ok()??;
                    if !matches!(
                        event_type.as_str(),
                        "m.room.message" | "m.room.encrypted" | "m.sticker"
                    ) {
                        return None;
                    }
                    let sender_id: String = raw.get_field("sender").ok()??;
                    let event_id: String = raw.get_field("event_id").ok()??;
                    let content: Option<Value> = raw.get_field("content").ok()?;
                    let relation = content
                        .as_ref()
                        .and_then(|content| content.get("m.relates_to"));
                    let relation_type = relation
                        .and_then(|relation| relation.get("rel_type"))
                        .and_then(Value::as_str);
                    if relation_type == Some("m.replace") {
                        return None;
                    }
                    // Relations stay in the clear part of encrypted events.
                    let thread_root = (relation_type == Some("m.thread"))
                        .then(|| relation.and_then(|relation| relation.get("event_id")))
                        .flatten()
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let body = content
                        .as_ref()
                        .and_then(|content| content.get("body"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let timestamp_ms: u64 = raw.get_field("origin_server_ts").ok()??;
                    (
                        Kind::Message,
                        sender_id,
                        format!("{}:{room_id}:{event_id}", account.id),
                        Some(event_id),
                        thread_root,
                        Some(timestamp_ms),
                        body,
                    )
                }
                RawAnySyncOrStrippedTimelineEvent::Stripped(raw) => {
                    let event_type: String = raw.get_field("type").ok()??;
                    if event_type != "m.room.member" {
                        return None;
                    }
                    let content: Value = raw.get_field("content").ok()??;
                    if content.get("membership").and_then(Value::as_str) != Some("invite") {
                        return None;
                    }
                    let sender_id: String = raw.get_field("sender").ok()??;
                    (
                        Kind::Invite,
                        sender_id.clone(),
                        format!("{}:{room_id}:invite:{sender_id}", account.id),
                        None,
                        None,
                        None,
                        None,
                    )
                }
            };
        if sender_id == account.user_id {
            return None;
        }
        Some(Self {
            account_id: account.id.clone(),
            account_user_id: account.user_id.clone(),
            room_id: room_id.into(),
            room_name: room_id.into(),
            sender_id,
            event_key,
            event_id,
            thread_root,
            timestamp_ms,
            kind,
            body,
            sdk_highlight,
            sdk_sound,
        })
    }
}

/// A level chosen in this window decides delivery only while the SDK holds
/// another level for the room (or its rule cannot be read). Once sync has
/// brought it to the SDK, the SDK's push actions decide: a keyword match
/// notifies without a highlight, which this window cannot tell apart.
/// Plan one banner whose room rule may first need a lookup of up to 30 s.
/// What can change meanwhile, the active account, the window's privacy screens
/// (sign-in, the app password, Manage accounts, a recovery key), the accepted
/// policy, the room mode accepted in this window and the clock that ages the
/// message, is read only after that lookup and after the last await, so a
/// ceiling raised, a Mute accepted or the age passed during the lookup
/// applies to this banner. The ignore list is judged in the same last
/// snapshot, read just before the account and the window (see
/// `final_snapshot`): only a sender an authoritative read allows gets a
/// banner, so an ignore added after enqueue, or a list that became
/// unreadable, drops it. Both preflights, before queuing and right before the
/// operating system takes the banner, plan this way.
async fn plan_after_lookup<S>(
    candidate: &Candidate,
    noted: impl Fn() -> Option<NotificationMode>,
    lookup: impl Future<Output = Option<NotificationMode>>,
    snapshot: impl FnOnce() -> S,
    config: impl FnOnce() -> Config,
    current: impl Fn() -> bool,
    now_ms: impl FnOnce() -> u64,
) -> Option<PlannedNotification>
where
    S: Future<Output = Option<(AccountInfo, UiSnapshot, SenderIgnore)>>,
{
    let held = match noted() {
        Some(_) => lookup.await,
        None => None,
    };
    if !current() {
        return None;
    }
    let (active, ui, ignore) = snapshot().await?;
    if !current() || !ui.live || ui.account_id != candidate.account_id {
        return None;
    }
    if ignore != SenderIgnore::Allowed {
        return None;
    }
    let mut config = config();
    privacy_ceiling(&mut config, ui.private);
    // The latest accepted room mode and the clock, after every await.
    plan_notification(
        candidate,
        &active,
        &config,
        ui.visible_room.as_ref(),
        noted_override(noted(), held),
        now_ms(),
    )
}

/// The last snapshot of a plan. The ignore list is read first; the active
/// account and then the window (live, the privacy screens, the visible room)
/// only after it, so whatever changed while the ignore list was read applies
/// to this banner. Each source is a future that does nothing until awaited.
async fn final_snapshot(
    ignore: impl Future<Output = SenderIgnore>,
    active: impl Future<Output = Option<AccountInfo>>,
    ui: impl Future<Output = Option<UiSnapshot>>,
) -> Option<(AccountInfo, UiSnapshot, SenderIgnore)> {
    let ignore = ignore.await;
    let active = active.await?;
    let ui = ui.await?;
    Some((active, ui, ignore))
}

fn noted_override(
    noted: Option<NotificationMode>,
    held: Option<NotificationMode>,
) -> Option<NotificationMode> {
    // A queued event's push actions may predate the accepted Mute even when
    // the SDK's current copy already holds it. Mute remains a final ceiling.
    noted.filter(|mode| *mode == NotificationMode::Mute || held != Some(*mode))
}

fn plan_notification(
    candidate: &Candidate,
    active: &AccountInfo,
    config: &Config,
    visible: Option<&VisibleRoom>,
    room_mode: Option<NotificationMode>,
    now_ms: u64,
) -> Option<PlannedNotification> {
    // A room rule just changed in this window applies before sync returns
    // it. Invitations are for rooms not joined yet and have no room rule.
    let room_allows = candidate.kind == Kind::Invite
        || match room_mode {
            Some(NotificationMode::Mute) => false,
            Some(NotificationMode::MentionsAndKeywords) => candidate.sdk_highlight,
            Some(NotificationMode::AllMessages) | None => true,
        };
    if !room_allows
        || candidate.account_id != active.id
        || candidate.account_user_id != active.user_id
        || candidate.sender_id == active.user_id
        || !config.notifications_enabled_for(&candidate.account_id)
        || visible.is_some_and(|visible| {
            visible.account_id == candidate.account_id && visible.room_id == candidate.room_id
        })
        || candidate.timestamp_ms.is_some_and(|timestamp| {
            now_ms.saturating_sub(timestamp) > MAX_NOTIFICATION_AGE.as_millis() as u64
        })
    {
        return None;
    }

    let room = safe_text(&candidate.room_name, 64);
    let sender = safe_text(sender_localpart(&candidate.sender_id), 48);
    let account = safe_text(&candidate.account_user_id, 64);
    // This room's own local behavior can only remove sound, preview or
    // local highlights; the global switches stay the ceiling.
    let local = config.room_notification_local(&candidate.account_id, &candidate.room_id);
    let highlight = candidate.sdk_highlight
        || (!local.no_highlights
            && candidate.body.as_ref().is_some_and(|body| {
                let body = body.to_lowercase();
                config.notifications.highlight_words.iter().any(|word| {
                    let word = word.trim().to_lowercase();
                    !word.is_empty() && body.contains(&word)
                })
            }));
    // With room and sender hidden, a banner names only the account, so it
    // still says which account a click opens.
    let identity = config.notifications.show_room_and_sender;
    let title = if identity {
        format!("{room} · {account}")
    } else {
        account
    };
    let summary = if highlight {
        format!("{title} · Highlight")
    } else {
        title
    };
    let body = match candidate.kind {
        Kind::Invite if identity => format!("Invitation from {sender}"),
        Kind::Invite => "New invitation".to_owned(),
        Kind::Message
            if identity && config.notifications.show_message_preview && !local.hide_preview =>
        {
            candidate.body.as_ref().map_or_else(
                || format!("{sender}: New message"),
                |body| format!("{sender}: {}", safe_text(body, MAX_TEXT_CHARS)),
            )
        }
        Kind::Message if identity => format!("{sender}: New message"),
        Kind::Message => "New message".to_owned(),
    };
    Some(PlannedNotification {
        summary,
        body,
        sound: config.notifications.sounds && candidate.sdk_sound && !local.quiet,
    })
}

fn sender_localpart(user_id: &str) -> &str {
    user_id
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or(user_id)
}

fn safe_text(input: &str, max_chars: usize) -> String {
    let mut result = String::new();
    let mut previous_space = false;
    for character in input.chars().take(max_chars) {
        // Matrix room names and message bodies are untrusted. Remove format
        // controls that can reorder or hide the adjacent account label.
        if matches!(
            character,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        ) {
            continue;
        }
        let character = match character {
            '<' => '‹',
            '>' => '›',
            control if control.is_control() || control.is_whitespace() => ' ',
            other => other,
        };
        if character == ' ' {
            if previous_space {
                continue;
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
        result.push(character);
    }
    result.trim().to_owned()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::{
        ruma::{events::AnySyncTimelineEvent, push::Action, serde::Raw},
        sync::Notification as MatrixNotification,
    };

    fn account(id: &str, user_id: &str) -> AccountInfo {
        AccountInfo {
            id: id.into(),
            user_id: user_id.into(),
            device_id: "DEVICE".into(),
            homeserver: "https://example.org".into(),
        }
    }

    /// A window and ignore-list snapshot as `plan_after_lookup` reads it.
    fn ready(
        active: &AccountInfo,
        ui: UiSnapshot,
        ignore: SenderIgnore,
    ) -> std::future::Ready<Option<(AccountInfo, UiSnapshot, SenderIgnore)>> {
        std::future::ready(Some((active.clone(), ui, ignore)))
    }

    fn candidate() -> Candidate {
        Candidate {
            account_id: "alice".into(),
            account_user_id: "@alice:example.org".into(),
            room_id: "!room:example.org".into(),
            room_name: "Work <chat>".into(),
            sender_id: "@bob:example.org".into(),
            event_key: "alice:!room:example.org:$event".into(),
            event_id: Some("$event".into()),
            thread_root: None,
            timestamp_ms: Some(1_000_000),
            kind: Kind::Message,
            body: Some("The secret <code> is ready".into()),
            sdk_highlight: false,
            sdk_sound: true,
        }
    }

    fn banner(token: u64, account_id: &str, user_id: &str, room_id: &str) -> ShownBanner {
        ShownBanner {
            token,
            target: NavigationTarget::new(account_id, room_id, Some("$event"), None).unwrap(),
            account_user_id: user_id.into(),
            shown_at: Instant::now(),
            withdrawal: None,
        }
    }

    #[test]
    fn active_account_visible_room_and_own_message_are_suppressed() {
        let active = account("alice", "@alice:example.org");
        let config = Config::default();
        let event = candidate();
        assert!(plan_notification(&event, &active, &config, None, None, 1_000_000).is_some());
        assert!(
            plan_notification(
                &event,
                &account("carol", "@carol:other.org"),
                &config,
                None,
                None,
                1_000_000
            )
            .is_none()
        );
        let visible = VisibleRoom {
            account_id: "alice".into(),
            room_id: "!room:example.org".into(),
        };
        assert!(
            plan_notification(&event, &active, &config, Some(&visible), None, 1_000_000).is_none()
        );
        let mut own = event;
        own.sender_id = active.user_id.clone();
        assert!(plan_notification(&own, &active, &config, None, None, 1_000_000).is_none());
    }

    #[test]
    fn privacy_sound_config_and_stale_events_are_respected() {
        let active = account("alice", "@alice:example.org");
        let mut config = Config::default();
        let event = candidate();
        let plan = plan_notification(&event, &active, &config, None, None, 1_000_000).unwrap();
        assert!(!plan.body.contains("secret"));
        assert!(plan.sound);
        config.notifications.show_message_preview = true;
        config.notifications.sounds = false;
        config.notifications.highlight_words.push("secret".into());
        let plan = plan_notification(&event, &active, &config, None, None, 1_000_000).unwrap();
        assert!(plan.body.contains("secret ‹code›"));
        assert!(plan.summary.contains("Highlight"));
        assert!(!plan.sound);
        assert!(
            plan_notification(&event, &active, &config, None, None, 1_000_000 + 301_000).is_none()
        );
        config
            .account_overrides
            .entry("alice".into())
            .or_default()
            .notifications_enabled = Some(false);
        assert!(plan_notification(&event, &active, &config, None, None, 1_000_000).is_none());
    }

    #[test]
    fn a_rooms_local_behavior_only_removes_sound_preview_and_local_highlights() {
        let active = account("alice", "@alice:example.org");
        let mut config = Config::default();
        config.notifications.show_message_preview = true;
        config.notifications.highlight_words.push("secret".into());
        let event = candidate();
        let all = morpheus_config::RoomNotificationLocal {
            quiet: true,
            hide_preview: true,
            no_highlights: true,
        };
        config.set_room_notification_local("alice", "!room:example.org", all);
        let plan = plan_notification(&event, &active, &config, None, None, 1_000_000).unwrap();
        assert!(!plan.sound);
        assert!(!plan.body.contains("secret"), "{}", plan.body);
        assert!(!plan.summary.contains("Highlight"));
        // Matrix mentions and keywords still mark it.
        let mut mention = candidate();
        mention.sdk_highlight = true;
        let plan = plan_notification(&mention, &active, &config, None, None, 1_000_000).unwrap();
        assert!(plan.summary.contains("Highlight"));
        // Another room, and the same room of another account, are unchanged.
        let mut other = candidate();
        other.room_id = "!other:example.org".into();
        let plan = plan_notification(&other, &active, &config, None, None, 1_000_000).unwrap();
        assert!(plan.sound && plan.body.contains("secret"));
        // A room's behavior never raises a global ceiling.
        config.set_room_notification_local("alice", "!room:example.org", Default::default());
        config.notifications.sounds = false;
        config.notifications.show_message_preview = false;
        let plan = plan_notification(&event, &active, &config, None, None, 1_000_000).unwrap();
        assert!(!plan.sound && !plan.body.contains("secret"));
    }

    #[tokio::test]
    async fn a_ceiling_raised_during_the_rule_lookup_applies_to_that_banner() {
        let active = account("alice", "@alice:example.org");
        let mut shown = Config::default();
        shown.notifications.show_message_preview = true;
        let config = std::sync::Mutex::new(shown);
        let ui = std::sync::Mutex::new(UiSnapshot {
            account_id: "alice".into(),
            live: true,
            visible_room: None,
            private: false,
        });
        let event = candidate();
        let (reached, suspended) = oneshot::channel::<()>();
        let (release, released) = oneshot::channel::<()>();
        // The room rule lookup waits until the test releases it.
        let lookup = async move {
            let _ = reached.send(());
            let _ = released.await;
            Some(NotificationMode::AllMessages)
        };
        let plan = plan_after_lookup(
            &event,
            || Some(NotificationMode::AllMessages),
            lookup,
            || ready(&active, ui.lock().unwrap().clone(), SenderIgnore::Allowed),
            || config.lock().unwrap().clone(),
            || true,
            || 1_000_000,
        );
        let change = async {
            suspended.await.unwrap();
            // While it waits: previews are turned off, then Manage accounts
            // and the app password screen open.
            config.lock().unwrap().notifications.show_message_preview = false;
            ui.lock().unwrap().private = true;
            release.send(()).unwrap();
        };
        let (plan, ()) = tokio::join!(plan, change);
        let plan = plan.expect("the banner is still shown, under the new ceiling");
        assert_eq!(plan.summary, "@alice:example.org");
        assert_eq!(plan.body, "New message");
        assert!(!plan.body.contains("secret") && !plan.summary.contains("Work"));
        // A selection lost during the lookup shows nothing at all.
        let (reached, suspended) = oneshot::channel::<()>();
        let (release, released) = oneshot::channel::<()>();
        let lost = std::sync::atomic::AtomicBool::new(false);
        let lookup = async move {
            let _ = reached.send(());
            let _ = released.await;
            None
        };
        let plan = plan_after_lookup(
            &event,
            || Some(NotificationMode::AllMessages),
            lookup,
            || ready(&active, ui.lock().unwrap().clone(), SenderIgnore::Allowed),
            || config.lock().unwrap().clone(),
            || !lost.load(Ordering::Acquire),
            || 1_000_000,
        );
        let change = async {
            suspended.await.unwrap();
            lost.store(true, Ordering::Release);
            release.send(()).unwrap();
        };
        let (plan, ()) = tokio::join!(plan, change);
        assert!(plan.is_none());
    }

    #[tokio::test]
    async fn a_mute_accepted_or_the_age_passed_during_the_lookup_governs_the_plan() {
        let active = account("alice", "@alice:example.org");
        let config = Config::default();
        let ui = UiSnapshot {
            account_id: "alice".into(),
            live: true,
            visible_room: None,
            private: false,
        };
        let event = candidate();
        let sent_at = event.timestamp_ms.unwrap();
        // Delay the room rule lookup until the change below is made.
        async fn delayed(
            reached: oneshot::Sender<()>,
            released: oneshot::Receiver<()>,
        ) -> Option<NotificationMode> {
            let _ = reached.send(());
            let _ = released.await;
            Some(NotificationMode::AllMessages)
        }
        // A Mute accepted in this window while the lookup waits.
        let noted = std::sync::Mutex::new(Some(NotificationMode::AllMessages));
        let (reached, suspended) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let plan = plan_after_lookup(
            &event,
            || *noted.lock().unwrap(),
            delayed(reached, released),
            || ready(&active, ui.clone(), SenderIgnore::Allowed),
            || config.clone(),
            || true,
            || sent_at,
        );
        let mute = async {
            suspended.await.unwrap();
            *noted.lock().unwrap() = Some(NotificationMode::Mute);
            release.send(()).unwrap();
        };
        let (plan, ()) = tokio::join!(plan, mute);
        assert!(
            plan.is_none(),
            "the Mute accepted during the lookup applies"
        );
        // The same delayed acceptance must win when the lookup has already
        // caught up to Mute, not only when it still returns AllMessages.
        let noted = std::sync::Mutex::new(Some(NotificationMode::AllMessages));
        let (reached, suspended) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let lookup = async {
            reached.send(()).unwrap();
            released.await.unwrap();
            Some(NotificationMode::Mute)
        };
        let plan = plan_after_lookup(
            &event,
            || *noted.lock().unwrap(),
            lookup,
            || ready(&active, ui.clone(), SenderIgnore::Allowed),
            || config.clone(),
            || true,
            || sent_at,
        );
        let mute = async {
            suspended.await.unwrap();
            *noted.lock().unwrap() = Some(NotificationMode::Mute);
            release.send(()).unwrap();
        };
        let (plan, ()) = tokio::join!(plan, mute);
        assert!(
            plan.is_none(),
            "accepted Mute also suppresses a lookup already holding Mute"
        );
        // The message's age passes while the lookup waits.
        let clock = AtomicU64::new(sent_at);
        let (reached, suspended) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let plan = plan_after_lookup(
            &event,
            || Some(NotificationMode::AllMessages),
            delayed(reached, released),
            || ready(&active, ui.clone(), SenderIgnore::Allowed),
            || config.clone(),
            || true,
            || clock.load(Ordering::Acquire),
        );
        let expire = async {
            suspended.await.unwrap();
            let late = sent_at + MAX_NOTIFICATION_AGE.as_millis() as u64 + 1;
            clock.store(late, Ordering::Release);
            release.send(()).unwrap();
        };
        let (plan, ()) = tokio::join!(plan, expire);
        assert!(plan.is_none(), "the age is judged at the final plan");
        // Without either change the same banner is planned.
        let (reached, suspended) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let plan = plan_after_lookup(
            &event,
            || Some(NotificationMode::AllMessages),
            delayed(reached, released),
            || ready(&active, ui.clone(), SenderIgnore::Allowed),
            || config.clone(),
            || true,
            || sent_at,
        );
        let wait = async {
            suspended.await.unwrap();
            release.send(()).unwrap();
        };
        let (plan, ()) = tokio::join!(plan, wait);
        assert!(plan.is_some());
    }

    #[test]
    fn an_unreadable_ignore_list_pauses_banners_until_a_successful_read() {
        // Only an authoritative read allows a banner.
        assert_eq!(sender_ignore(Ok(false)), SenderIgnore::Allowed);
        assert_eq!(sender_ignore(Ok(true)), SenderIgnore::Dropped);
        for unreadable in [
            MemberPrivacyError::Unavailable,
            MemberPrivacyError::TimedOut,
        ] {
            assert_eq!(sender_ignore(Err(unreadable)), SenderIgnore::Paused);
        }
        assert_eq!(
            sender_ignore(Err(MemberPrivacyError::AccountChanged)),
            SenderIgnore::Stale
        );
        let mut pause = IgnorePause::default();
        assert_eq!(pause.observe("work", SenderIgnore::Paused), Some(true));
        assert_eq!(pause.observe("work", SenderIgnore::Paused), None);
        assert!(pause.paused("work") && !pause.paused("home"));
        // A stale read is no evidence; a successful read resumes, whatever it
        // says about the sender.
        assert_eq!(pause.observe("work", SenderIgnore::Stale), None);
        assert!(pause.paused("work"));
        assert_eq!(pause.observe("work", SenderIgnore::Dropped), Some(false));
        assert!(!pause.paused("work"));
        assert_eq!(pause.observe("work", SenderIgnore::Allowed), None);
    }

    #[test]
    fn only_the_latest_change_posts_and_it_shows_the_state_as_it_is() {
        // What record_ignore does under its lock: observe, and number the
        // change it posts for.
        let pause = Arc::new(Mutex::new(IgnorePause::default()));
        let record = |account: &str, gate| {
            let mut state = pause.lock().unwrap();
            state.observe(account, gate).map(|_| state.version(account))
        };
        // Two reads post their changes, paused and then resumed, and the
        // posts run in the other order. The newer one shows the state as it
        // is; the older one, run last, shows nothing, so "paused" never comes
        // back.
        let paused = record("work", SenderIgnore::Paused).unwrap();
        let resumed = record("work", SenderIgnore::Allowed).unwrap();
        let older = line_post(pause.clone(), "work".into(), paused);
        let newer = line_post(pause.clone(), "work".into(), resumed);
        assert_eq!(newer("work"), Some(false));
        assert_eq!(older("work"), None);
        // A read that changes nothing makes no post.
        assert_eq!(record("work", SenderIgnore::Stale), None);
        // A post for an account not on screen shows nothing. The state is per
        // account: after A to B and back to A, A's latest post still shows A's
        // state as it is, whatever another account did meanwhile.
        let elsewhere = line_post(pause.clone(), "work".into(), resumed);
        assert_eq!(elsewhere("home"), None);
        assert!(record("home", SenderIgnore::Paused).is_some());
        let back = line_post(pause.clone(), "work".into(), resumed);
        assert_eq!(back("work"), Some(false));
    }

    #[tokio::test]
    async fn the_ignore_list_and_settings_are_judged_at_final_delivery() {
        let active = account("alice", "@alice:example.org");
        let ui = UiSnapshot {
            account_id: "alice".into(),
            live: true,
            visible_room: None,
            private: false,
        };
        let event = candidate();
        let plan = |ignore| {
            let snapshot = ready(&active, ui.clone(), ignore);
            plan_after_lookup(
                &event,
                || None,
                async { None },
                || snapshot,
                Config::default,
                || true,
                || 1_000_000,
            )
        };
        assert!(plan(SenderIgnore::Allowed).await.is_some());
        // Unreadable list, ignored sender, stale selection: no banner.
        for ignore in [
            SenderIgnore::Paused,
            SenderIgnore::Dropped,
            SenderIgnore::Stale,
        ] {
            assert!(plan(ignore).await.is_none(), "{ignore:?}");
        }
        // Held until delivery: an ignore added, or notifications turned off,
        // after the banner was queued drops it.
        async fn delayed(
            reached: oneshot::Sender<()>,
            released: oneshot::Receiver<()>,
        ) -> Option<NotificationMode> {
            let _ = reached.send(());
            let _ = released.await;
            Some(NotificationMode::AllMessages)
        }
        let ignore = std::sync::Mutex::new(SenderIgnore::Allowed);
        let settings = std::sync::Mutex::new(Config::default());
        for change in 0..2 {
            *ignore.lock().unwrap() = SenderIgnore::Allowed;
            *settings.lock().unwrap() = Config::default();
            let (reached, suspended) = oneshot::channel();
            let (release, released) = oneshot::channel();
            let plan = plan_after_lookup(
                &event,
                || Some(NotificationMode::AllMessages),
                delayed(reached, released),
                || ready(&active, ui.clone(), *ignore.lock().unwrap()),
                || settings.lock().unwrap().clone(),
                || true,
                || 1_000_000,
            );
            let after_enqueue = async {
                suspended.await.unwrap();
                if change == 0 {
                    *ignore.lock().unwrap() = SenderIgnore::Dropped;
                } else {
                    settings.lock().unwrap().notifications.enabled = false;
                }
                release.send(()).unwrap();
            };
            let (plan, ()) = tokio::join!(plan, after_enqueue);
            assert!(
                plan.is_none(),
                "change {change} after enqueue was not honoured"
            );
        }
    }

    /// The production last snapshot (`final_snapshot`, as `plan_current`
    /// calls it). While the ignore list is still being read, the window moves
    /// to another account, the session ends, or a privacy screen opens. The
    /// window is read after the ignore list, so its new state decides.
    #[tokio::test]
    async fn the_window_is_read_after_a_delayed_ignore_read() {
        let active = account("alice", "@alice:example.org");
        let event = candidate();
        let alice = UiSnapshot {
            account_id: "alice".into(),
            live: true,
            visible_room: None,
            private: false,
        };
        let window = std::sync::Mutex::new(alice.clone());
        let mut config = Config::default();
        config.notifications.show_message_preview = true;
        for change in 0..3 {
            *window.lock().unwrap() = alice.clone();
            let (reached, suspended) = oneshot::channel();
            let (release, released) = oneshot::channel();
            let ignore = async {
                let _ = reached.send(());
                let _ = released.await;
                SenderIgnore::Allowed
            };
            let ui = async { Some(window.lock().unwrap().clone()) };
            let plan = plan_after_lookup(
                &event,
                || None,
                async { None },
                || final_snapshot(ignore, std::future::ready(Some(active.clone())), ui),
                || config.clone(),
                || true,
                || 1_000_000,
            );
            let meanwhile = async {
                suspended.await.unwrap();
                {
                    let mut shown = window.lock().unwrap();
                    match change {
                        0 => shown.account_id = "bob".into(),
                        1 => shown.live = false,
                        _ => shown.private = true,
                    }
                }
                release.send(()).unwrap();
            };
            let (plan, ()) = tokio::join!(plan, meanwhile);
            if change < 2 {
                assert!(
                    plan.is_none(),
                    "change {change} during the ignore read was ignored"
                );
            } else {
                // A privacy screen opened meanwhile: the banner names only
                // the account.
                let plan = plan.expect("a banner naming only the account");
                assert_eq!(plan.summary, "@alice:example.org");
                assert_eq!(plan.body, "New message");
            }
        }
    }

    #[tokio::test]
    async fn an_ignore_list_over_the_byte_cap_keeps_banners_paused() {
        // The strict reader refuses a stored list over its byte cap before
        // parsing it, as Unavailable (morpheus-matrix member_privacy tests).
        // However often it is read again, banners stay paused and none is
        // planned; only a later read within the cap resumes them.
        let over_cap = || sender_ignore(Err(MemberPrivacyError::Unavailable));
        assert_eq!(over_cap(), SenderIgnore::Paused);
        let mut pause = IgnorePause::default();
        assert_eq!(pause.observe("alice", over_cap()), Some(true));
        for _ in 0..3 {
            assert_eq!(pause.observe("alice", over_cap()), None);
            assert!(pause.paused("alice"));
        }
        let active = account("alice", "@alice:example.org");
        let ui = UiSnapshot {
            account_id: "alice".into(),
            live: true,
            visible_room: None,
            private: false,
        };
        let event = candidate();
        let plan = plan_after_lookup(
            &event,
            || None,
            async { None },
            || ready(&active, ui.clone(), over_cap()),
            Config::default,
            || true,
            || 1_000_000,
        );
        assert!(plan.await.is_none());
        assert_eq!(
            pause.observe("alice", sender_ignore(Ok(false))),
            Some(false)
        );
        assert!(!pause.paused("alice"));
    }

    #[test]
    fn account_access_and_security_screens_hide_room_sender_and_text() {
        let active = account("alice", "@alice:example.org");
        let mut config = Config::default();
        config.notifications.show_message_preview = true;
        privacy_ceiling(&mut config, false);
        let plan =
            plan_notification(&candidate(), &active, &config, None, None, 1_000_000).unwrap();
        assert!(plan.body.contains("secret"));
        privacy_ceiling(&mut config, true);
        let plan =
            plan_notification(&candidate(), &active, &config, None, None, 1_000_000).unwrap();
        assert_eq!(plan.summary, "@alice:example.org");
        assert_eq!(plan.body, "New message");
    }

    #[test]
    fn hidden_identity_names_only_the_account_and_never_previews() {
        let active = account("alice", "@alice:example.org");
        let mut config = Config::default();
        config.notifications.show_message_preview = true;
        config.notifications.show_room_and_sender = false;
        let plan =
            plan_notification(&candidate(), &active, &config, None, None, 1_000_000).unwrap();
        assert_eq!(plan.summary, "@alice:example.org");
        assert_eq!(plan.body, "New message");
        let mut invite = candidate();
        invite.kind = Kind::Invite;
        invite.timestamp_ms = None;
        let plan = plan_notification(&invite, &active, &config, None, None, 1_000_000).unwrap();
        assert_eq!(plan.body, "New invitation");
        assert!(!plan.summary.contains("Work") && !plan.body.contains("bob"));
    }

    #[test]
    fn a_room_rule_changed_here_applies_before_sync_returns_it() {
        let active = account("alice", "@alice:example.org");
        let config = Config::default();
        let mut event = candidate();
        let mute = Some(NotificationMode::Mute);
        let mentions = Some(NotificationMode::MentionsAndKeywords);
        assert!(plan_notification(&event, &active, &config, None, mute, 1_000_000).is_none());
        assert!(plan_notification(&event, &active, &config, None, mentions, 1_000_000).is_none());
        event.sdk_highlight = true;
        assert!(plan_notification(&event, &active, &config, None, mentions, 1_000_000).is_some());
        assert!(plan_notification(&event, &active, &config, None, mute, 1_000_000).is_none());
    }

    #[test]
    fn a_level_chosen_here_decides_only_until_the_sdk_holds_it() {
        let mentions = Some(NotificationMode::MentionsAndKeywords);
        let all = Some(NotificationMode::AllMessages);
        // Before sync returns it, the chosen level overrides the SDK.
        assert_eq!(noted_override(mentions, all), mentions);
        assert_eq!(noted_override(mentions, None), mentions);
        // Once the SDK holds it, its push actions decide, keywords included.
        assert_eq!(noted_override(mentions, mentions), None);
        assert_eq!(noted_override(None, all), None);
        let mute = Some(NotificationMode::Mute);
        assert_eq!(
            noted_override(mute, mute),
            mute,
            "queued actions can predate Mute"
        );
    }

    #[test]
    fn shown_banners_resolve_once_and_never_for_removed_accounts() {
        let mut shown = ShownBanners::default();
        shown.insert(banner(1, "alice", "@alice:example.org", "!a:example.org"));
        shown.insert(banner(2, "bob", "@bob:example.org", "!b:example.org"));
        // A banner resolves exactly once: a repeated click is stale.
        assert_eq!(
            shown
                .take(1, Instant::now())
                .map(|banner| banner.target.room_id().to_owned()),
            Some("!a:example.org".into())
        );
        assert!(shown.take(1, Instant::now()).is_none());
        // Removing bob, or re-adding the ID for another Matrix user, forgets
        // his banner instead of reviving the account later.
        shown.insert(banner(3, "alice", "@alice:example.org", "!a:example.org"));
        let saved = [
            account("alice", "@alice:example.org"),
            account("bob", "@mallory:example.org"),
        ];
        shown.retain_accounts(Some(saved.as_slice()));
        assert!(shown.take(2, Instant::now()).is_none());
        // Expired records resolve to nothing.
        assert!(shown.take(3, Instant::now() + SHOWN_TTL).is_none());
        // The record stays bounded.
        for token in 10..(10 + MAX_SHOWN as u64 + 5) {
            shown.insert(banner(
                token,
                "alice",
                "@alice:example.org",
                "!a:example.org",
            ));
        }
        assert_eq!(shown.0.len(), MAX_SHOWN);
        assert!(shown.take(10, Instant::now()).is_none());
        shown.retain_accounts(None);
        assert!(shown.0.is_empty());
    }

    #[test]
    fn expired_and_oldest_withdrawable_banners_release_their_listeners() {
        let mut shown = ShownBanners::default();
        shown.insert(banner(1, "alice", "@alice:example.org", "!a:example.org"));
        let (withdrawal, mut oldest) = Withdrawal::for_test();
        let mut closable = banner(2, "alice", "@alice:example.org", "!b:example.org");
        closable.withdrawal = Some(withdrawal);
        shown.insert(closable);
        let (withdrawal, mut newest) = Withdrawal::for_test();
        let mut closable = banner(3, "alice", "@alice:example.org", "!c:example.org");
        closable.withdrawal = Some(withdrawal);
        shown.insert(closable);
        // The oldest banner that can be withdrawn goes first; one the
        // platform cannot close stays clickable.
        shown.withdraw_oldest();
        assert_eq!(oldest.try_recv(), Ok(()));
        assert!(newest.try_recv().is_err());
        let tokens: Vec<u64> = shown.0.iter().map(|banner| banner.token).collect();
        assert_eq!(tokens, [1, 3]);
        // Banners past their lifetime are withdrawn and forgotten.
        shown.prune(Instant::now() + SHOWN_TTL);
        assert!(shown.0.is_empty());
        assert_eq!(newest.try_recv(), Ok(()));
    }

    #[test]
    fn matrix_push_event_projection_keeps_plain_body_and_filters_edits() {
        let account = account("alice", "@alice:example.org");
        let event = serde_json::json!({
            "type": "m.room.message",
            "event_id": "$message:example.org",
            "sender": "@bob:example.org",
            "origin_server_ts": 1_000_000,
            "content": {"msgtype": "m.text", "body": "A plain-text body"}
        });
        let raw: Raw<AnySyncTimelineEvent> = serde_json::from_value(event.clone()).unwrap();
        let notification = MatrixNotification {
            actions: vec![Action::Notify],
            event: RawAnySyncOrStrippedTimelineEvent::Sync(raw),
        };
        let candidate =
            Candidate::from_matrix(&account, "!room:example.org", notification).unwrap();
        assert_eq!(candidate.body.as_deref(), Some("A plain-text body"));
        assert_eq!(candidate.sender_id, "@bob:example.org");
        assert_eq!(candidate.event_id.as_deref(), Some("$message:example.org"));
        assert_eq!(candidate.thread_root, None);

        let mut threaded = event.clone();
        threaded["content"]["m.relates_to"] =
            serde_json::json!({"rel_type": "m.thread", "event_id": "$root:example.org"});
        let raw: Raw<AnySyncTimelineEvent> = serde_json::from_value(threaded).unwrap();
        let notification = MatrixNotification {
            actions: vec![Action::Notify],
            event: RawAnySyncOrStrippedTimelineEvent::Sync(raw),
        };
        let candidate =
            Candidate::from_matrix(&account, "!room:example.org", notification).unwrap();
        assert_eq!(candidate.thread_root.as_deref(), Some("$root:example.org"));

        let mut edit = event;
        edit["content"]["m.relates_to"] =
            serde_json::json!({"rel_type": "m.replace", "event_id": "$older:example.org"});
        let raw: Raw<AnySyncTimelineEvent> = serde_json::from_value(edit).unwrap();
        let notification = MatrixNotification {
            actions: vec![Action::Notify],
            event: RawAnySyncOrStrippedTimelineEvent::Sync(raw),
        };
        assert!(Candidate::from_matrix(&account, "!room:example.org", notification).is_none());
    }

    #[test]
    fn untrusted_labels_cannot_reorder_the_account_name() {
        let mut event = candidate();
        event.room_name = "room\u{061c}\u{200f}\u{202e}\u{2060}\u{2069} name".into();
        let plan = plan_notification(
            &event,
            &account("alice", "@alice:example.org"),
            &Config::default(),
            None,
            None,
            1_000_000,
        )
        .unwrap();
        assert_eq!(plan.summary, "room name · @alice:example.org");
    }

    /// Incoming-call banners follow the explicit alert switches (global and
    /// per-account `notifications_enabled`), name the caller only where room
    /// and sender may be named and never on a private screen, and never carry
    /// a sound of their own: incoming_ring::audio alone rings.
    #[cfg(feature = "native-calls")]
    #[test]
    fn call_banners_follow_alert_and_privacy_settings_and_stay_silent() {
        let mut config = Config::default();
        config.notifications.sounds = true;
        assert_eq!(silent_identity(&config, "alice", false), Some(true));
        assert_eq!(silent_identity(&config, "alice", true), Some(false));
        config.notifications.show_room_and_sender = false;
        assert_eq!(silent_identity(&config, "alice", false), Some(false));
        config.notifications.show_room_and_sender = true;
        config
            .account_overrides
            .entry("alice".into())
            .or_default()
            .notifications_enabled = Some(false);
        assert_eq!(silent_identity(&config, "alice", false), None);
        assert_eq!(silent_identity(&config, "bob", false), Some(true));
        config.notifications.enabled = false;
        assert_eq!(silent_identity(&config, "bob", false), None);

        let (summary, body) = call_banner_text(
            "@alice:example.org",
            "@ca\u{202e}ller\u{200f}:example.org",
            true,
        );
        assert_eq!(summary, "Incoming call · @alice:example.org");
        assert_eq!(body, "caller is calling");
        let (_, hidden) = call_banner_text("@alice:example.org", "@caller:example.org", false);
        assert_eq!(hidden, "Open Morpheus to see the call");
        let caller = format!("@{}:example.org", "x".repeat(400));
        let (_, long) = call_banner_text("@alice:example.org", &caller, true);
        assert_eq!(long, format!("{} is calling", "x".repeat(48)));
        let plan = silent_plan(&summary, &format!("{body}\n<b>"));
        assert!(!plan.sound);
        assert_eq!(plan.body, "caller is calling ‹b›");
    }
}
