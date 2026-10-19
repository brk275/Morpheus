//! Account-scoped desktop notifications from the Matrix SDK's push-rule feed.
//!
//! This module has no process-independent helper or scheduled delivery. The
//! queue and all handlers live only for the lifetime of Morpheus.
//!
//! A shown banner is remembered only as a bounded record of identifiers (an
//! opaque token, the account, room, event and thread IDs) so a click can open
//! it and reading the room can withdraw it. No message text is kept after the
//! banner is handed to the OS, and nothing is persisted.

mod presenter;

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use matrix_sdk::{
    Room, deserialized_responses::RawAnySyncOrStrippedTimelineEvent,
    sync::Notification as MatrixNotification,
};
use morpheus_config::Config;
use morpheus_matrix::{AccountInfo, AccountManager, AccountSelection, NotificationMode};
use serde_json::Value;
use slint::{ComponentHandle, Weak as UiWeak};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
};

use self::presenter::{PresenterEvent, Withdrawal};
use crate::{AppWindow, navigation_target::NavigationTarget};

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

struct UiSnapshot {
    account_id: String,
    live: bool,
    visible_room: Option<VisibleRoom>,
}

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

#[derive(Debug, PartialEq, Eq)]
struct PlannedNotification {
    summary: String,
    body: String,
    sound: bool,
}

struct QueuedNotification {
    candidate: Candidate,
    selection: AccountSelection,
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
    stopped: AtomicBool,
    queued: mpsc::Sender<QueuedNotification>,
    shown: Mutex<ShownBanners>,
    next_token: AtomicU64,
    room_modes: Mutex<HashMap<(String, String), (NotificationMode, Instant)>>,
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
        let (queued, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let (presenter_events, presenter_receiver) = mpsc::channel(MAX_SHOWN);
        let (activation_tx, activations) = mpsc::channel(ACTIVATION_CAPACITY);
        let service = Arc::new(Self {
            manager: manager.clone(),
            config: RwLock::new(config),
            ui,
            stopped: AtomicBool::new(false),
            queued,
            shown: Mutex::new(ShownBanners::default()),
            next_token: AtomicU64::new(1),
            room_modes: Mutex::new(HashMap::new()),
            presenter_events,
            activation_tx,
            activations: Mutex::new(Some(activations)),
        });
        runtime.spawn(watch_accounts(manager.clone(), Arc::downgrade(&service)));
        runtime.spawn(deliver_queued(Arc::downgrade(&service), receiver));
        runtime.spawn(resolve_presenter_events(
            Arc::downgrade(&service),
            presenter_receiver,
        ));
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
        self.stopped.store(true, Ordering::Release);
        self.shown().retain_accounts(None);
    }

    /// Morpheus is quitting; no more notification work starts.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
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

    /// The room was read: withdraw its banners where the platform allows.
    pub(crate) fn withdraw_room(&self, account_id: &str, room_id: &str) {
        self.shown().withdraw_room(account_id, room_id);
    }

    /// Rooms of `account_id` with banners that reading would withdraw.
    pub(crate) fn withdrawable_rooms(&self, account_id: &str) -> Vec<String> {
        self.shown().withdrawable_rooms(account_id)
    }

    fn shown(&self) -> std::sync::MutexGuard<'_, ShownBanners> {
        self.shown.lock().unwrap_or_else(|error| error.into_inner())
    }

    async fn ui_snapshot(&self) -> Option<UiSnapshot> {
        let (send, receive) = oneshot::channel();
        self.ui
            .upgrade_in_event_loop(move |ui| {
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
                let _ = send.send(UiSnapshot {
                    account_id,
                    live,
                    visible_room,
                });
            })
            .ok()?;
        tokio::time::timeout(UI_QUERY_TIMEOUT, receive)
            .await
            .ok()?
            .ok()
    }

    async fn enqueue(
        &self,
        account: &AccountInfo,
        selection: AccountSelection,
        notification: MatrixNotification,
        room: Room,
    ) {
        if self.stopped.load(Ordering::Acquire)
            || !selection.is_current()
            || selection.account_id() != account.id
            || self
                .manager
                .active_account()
                .await
                .is_none_or(|active| active.id != account.id)
        {
            return;
        }
        let Some(mut candidate) =
            Candidate::from_matrix(account, room.room_id().as_str(), notification)
        else {
            return;
        };
        candidate.room_name = room
            .display_name()
            .await
            .map(|name| name.to_string())
            .unwrap_or_else(|_| candidate.room_id.clone());
        // Never wait for a desktop notification server inside SDK sync.
        if selection.is_current() {
            let _ = self.queued.try_send(QueuedNotification {
                candidate,
                selection,
            });
        }
    }

    /// Read the account, UI visibility, and current policy together as late as
    /// possible. A queued event must never carry an earlier account's preview
    /// setting into the native notification presenter.
    async fn plan_current(&self, queued: &QueuedNotification) -> Option<PlannedNotification> {
        let candidate = &queued.candidate;
        if self.stopped.load(Ordering::Acquire)
            || !queued.selection.is_current()
            || queued.selection.account_id() != candidate.account_id
        {
            return None;
        }
        let active = self.manager.active_account().await?;
        let ui = self.ui_snapshot().await?;
        if !ui.live || ui.account_id != candidate.account_id {
            return None;
        }
        let config = self
            .config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let noted = self.room_mode(&candidate.account_id, &candidate.room_id);
        let held = match noted {
            Some(_) => self
                .manager
                .room_notification_rule_selected(&queued.selection, &candidate.room_id)
                .await
                .ok()
                .map(|rule| rule.effective),
            None => None,
        };
        // Account changes can happen during any await above. Returning to
        // the same account must not revive this earlier notification.
        if self.stopped.load(Ordering::Acquire) || !queued.selection.is_current() {
            return None;
        }
        plan_notification(
            candidate,
            &active,
            &config,
            ui.visible_room.as_ref(),
            noted_override(noted, held),
            now_ms(),
        )
    }

    /// Resolve one clicked banner to its target. Only a remembered, unexpired
    /// banner of a still-saved account with the same Matrix user resolves; a
    /// removed account is never brought back by an old banner.
    async fn resolve_activation(&self, token: u64) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        let Some(banner) = self.shown().take(token, Instant::now()) else {
            return;
        };
        let accounts = self.manager.accounts().await;
        let pending_removal = self
            .manager
            .app_password_status()
            .await
            .pending_removals
            .iter()
            .any(|id| id == banner.target.account_id());
        let saved = accounts.iter().any(|account| {
            account.id == banner.target.account_id() && account.user_id == banner.account_user_id
        });
        if !saved || pending_removal || self.stopped.load(Ordering::Acquire) {
            withdraw(banner);
            return;
        }
        // Bounded: identifiers only, and a full queue drops this click.
        let _ = self.activation_tx.try_send(banner.target.clone());
    }
}

async fn watch_accounts(manager: Arc<AccountManager>, service: Weak<DesktopNotifications>) {
    let mut selected = manager.subscribe_active_account();
    loop {
        let account = selected.borrow().clone();
        if let Some(account) = account {
            let weak = service.clone();
            let callback_account = account.clone();
            // The manager rechecks the exact selected target and owns the
            // once-only registration slot for this SDK client's lifetime.
            let _ = manager
                .register_notification_handler_for(
                    &account.id,
                    move |notification, room, _client| {
                        let account = callback_account.clone();
                        // Capture at callback invocation, before the async body
                        // can be queued behind an A → B → A transition.
                        let service = weak.upgrade();
                        let selection = service
                            .as_ref()
                            .and_then(|service| service.manager.account_selection(&account.id));
                        async move {
                            if let (Some(service), Some(selection)) = (service, selection) {
                                service
                                    .enqueue(&account, selection, notification, room)
                                    .await;
                            }
                        }
                    },
                )
                .await;
        }
        // A removed or signed-out account's banners must not stay clickable.
        if let Some(service) = service.upgrade() {
            let saved = manager.accounts().await;
            service.shown().retain_accounts(Some(saved.as_slice()));
        }
        if service.upgrade().is_none() || selected.changed().await.is_err() {
            return;
        }
    }
}

async fn deliver_queued(
    service: Weak<DesktopNotifications>,
    mut receiver: mpsc::Receiver<QueuedNotification>,
) {
    let mut seen = HashMap::<String, Instant>::new();
    while let Some(queued) = receiver.recv().await {
        let Some(service) = service.upgrade() else {
            return;
        };
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

        // `show` can block on D-Bus or the OS notification service. Serialize
        // deliveries and keep the Matrix sync loop free to process events.
        let runtime = Handle::current();
        let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
            // A blocking worker may begin after account switching, room
            // navigation, or a Settings save. Query all three again directly
            // before handing the notification to the OS.
            let Some(plan) = runtime.block_on(service.plan_current(&queued)) else {
                return Ok(());
            };
            let candidate = queued.candidate;
            // Keep click handling for this banner: forget banners past their
            // lifetime and, with every listener taken, withdraw the oldest
            // banner that can be withdrawn, which releases its listener.
            {
                let mut shown = service.shown();
                shown.prune(Instant::now());
                if presenter::listeners_full() {
                    shown.withdraw_oldest();
                }
            }
            let token = service.next_token.fetch_add(1, Ordering::AcqRel);
            let withdrawal =
                presenter::show(plan, token, service.presenter_events.clone(), runtime)?;
            if service.stopped.load(Ordering::Acquire) {
                // Quitting: a banner shown meanwhile is withdrawn where possible.
                if let Some(withdrawal) = withdrawal {
                    withdrawal.withdraw();
                }
            } else {
                // One banner per room: once shown, a newer message replaces
                // the room's earlier banners where the platform can withdraw
                // them. A failed show leaves them in place.
                service.withdraw_room(&candidate.account_id, &candidate.room_id);
                if let Some(target) = target {
                    service.shown().insert(ShownBanner {
                        token,
                        target,
                        account_user_id: candidate.account_user_id,
                        shown_at: Instant::now(),
                        withdrawal,
                    });
                }
            }
            // Without a well-formed target the banner stays, and a click on
            // it resolves to nothing.
            Ok(())
        })
        .await;
        if let Ok(Err(error)) = result {
            eprintln!("Could not display a desktop notification: {error}");
        }
    }
}

async fn resolve_presenter_events(
    service: Weak<DesktopNotifications>,
    mut events: mpsc::Receiver<PresenterEvent>,
) {
    while let Some(event) = events.recv().await {
        let Some(service) = service.upgrade() else {
            return;
        };
        match event {
            PresenterEvent::Activated(token) => service.resolve_activation(token).await,
            // A dismissed banner can no longer be opened or withdrawn.
            PresenterEvent::Closed(token) => {
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
fn noted_override(
    noted: Option<NotificationMode>,
    held: Option<NotificationMode>,
) -> Option<NotificationMode> {
    noted.filter(|mode| held != Some(*mode))
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
    let highlight = candidate.sdk_highlight
        || candidate.body.as_ref().is_some_and(|body| {
            let body = body.to_lowercase();
            config.notifications.highlight_words.iter().any(|word| {
                let word = word.trim().to_lowercase();
                !word.is_empty() && body.contains(&word)
            })
        });
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
        Kind::Message if identity && config.notifications.show_message_preview => {
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
        sound: config.notifications.sounds && candidate.sdk_sound,
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
            shown.insert(banner(token, "alice", "@alice:example.org", "!a:example.org"));
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
}
