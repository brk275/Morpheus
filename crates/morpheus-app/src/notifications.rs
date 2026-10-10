//! Account-scoped desktop notifications from the Matrix SDK's push-rule feed.
//!
//! This module has no process-independent helper or scheduled delivery. The
//! queue and all handlers live only for the lifetime of Morpheus.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use matrix_sdk::{
    Room, deserialized_responses::RawAnySyncOrStrippedTimelineEvent,
    sync::Notification as MatrixNotification,
};
use morpheus_config::Config;
use morpheus_matrix::{AccountInfo, AccountManager};
use notify_rust::Notification;
use serde_json::Value;
use slint::{ComponentHandle, Weak as UiWeak};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
};

use crate::AppWindow;

const MAX_NOTIFICATION_AGE: Duration = Duration::from_secs(5 * 60);
const SEEN_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_TEXT_CHARS: usize = 180;
const QUEUE_CAPACITY: usize = 64;
const UI_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

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

/// The UI supplies the latest valid config. The worker asks Slint whether a
/// room is on screen at delivery time, so a navigation change cannot leave a
/// stale room suppression cache.
/// `watch_accounts` registers one Matrix notification callback for each loaded
/// account. Both the callback and the delivery worker re-check the active
/// account, so queued work from an earlier selection is discarded.
pub struct DesktopNotifications {
    manager: Arc<AccountManager>,
    config: RwLock<Config>,
    ui: UiWeak<AppWindow>,
    stopped: AtomicBool,
    queued: mpsc::Sender<Candidate>,
}

impl DesktopNotifications {
    pub fn start(
        manager: Arc<AccountManager>,
        config: Config,
        ui: UiWeak<AppWindow>,
        runtime: &Handle,
    ) -> Arc<Self> {
        let (queued, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let service = Arc::new(Self {
            manager: manager.clone(),
            config: RwLock::new(config),
            ui,
            stopped: AtomicBool::new(false),
            queued,
        });
        runtime.spawn(watch_accounts(manager.clone(), Arc::downgrade(&service)));
        runtime.spawn(deliver_queued(Arc::downgrade(&service), receiver));
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

    /// Call before stopping Matrix sync and leaving the UI event loop.
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    async fn ui_snapshot(&self) -> Option<UiSnapshot> {
        let (send, receive) = oneshot::channel();
        self.ui
            .upgrade_in_event_loop(move |ui| {
                let account_id = ui.get_active_account_local_id().to_string();
                let room_id = ui.get_active_room_id().to_string();
                let live = ui.get_live_session();
                let visible_room = if live
                    && ui.get_page() == 0
                    && ui.window().is_visible()
                    && !ui.window().is_minimized()
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

    async fn enqueue(&self, account: &AccountInfo, notification: MatrixNotification, room: Room) {
        if self.stopped.load(Ordering::Acquire)
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
        let _ = self.queued.try_send(candidate);
    }

    /// Read the account, UI visibility, and current policy together as late as
    /// possible. A queued event must never carry an earlier account's preview
    /// setting into the native notification presenter.
    async fn plan_current(&self, candidate: &Candidate) -> Option<PlannedNotification> {
        if self.stopped.load(Ordering::Acquire) {
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
        plan_notification(
            candidate,
            &active,
            &config,
            ui.visible_room.as_ref(),
            now_ms(),
        )
    }
}

async fn watch_accounts(manager: Arc<AccountManager>, service: Weak<DesktopNotifications>) {
    let mut selected = manager.subscribe_active_account();
    let mut registered = HashSet::new();
    loop {
        let account = selected.borrow().clone();
        if let Some(account) = account
            && !registered.contains(&account.id)
            && let Ok(client) = manager.active_client().await
            // The selected account may have changed while active_client ran.
            && client.user_id().is_some_and(|id| id.as_str() == account.user_id)
            && client.device_id().is_some_and(|id| id.as_str() == account.device_id)
        {
            let weak = service.clone();
            let callback_account = account.clone();
            client
                .register_notification_handler(move |notification, room, _client| {
                    let weak = weak.clone();
                    let account = callback_account.clone();
                    async move {
                        if let Some(service) = weak.upgrade() {
                            service.enqueue(&account, notification, room).await;
                        }
                    }
                })
                .await;
            registered.insert(account.id);
        }
        if service.upgrade().is_none() || selected.changed().await.is_err() {
            return;
        }
    }
}

async fn deliver_queued(
    service: Weak<DesktopNotifications>,
    mut receiver: mpsc::Receiver<Candidate>,
) {
    let mut seen = HashMap::<String, Instant>::new();
    while let Some(candidate) = receiver.recv().await {
        let Some(service) = service.upgrade() else {
            return;
        };
        if service.plan_current(&candidate).await.is_none() {
            continue;
        }
        let now = Instant::now();
        seen.retain(|_, observed| now.duration_since(*observed) < SEEN_TTL);
        if seen.insert(candidate.event_key.clone(), now).is_some() {
            continue;
        }

        // `show` can block on D-Bus or the OS notification service. Serialize
        // deliveries and keep the Matrix sync loop free to process events.
        let runtime = Handle::current();
        let result = tokio::task::spawn_blocking(move || {
            // A blocking worker may begin after account switching, room
            // navigation, or a Settings save. Query all three again directly
            // before handing the notification to the OS.
            if let Some(plan) = runtime.block_on(service.plan_current(&candidate)) {
                show_native(plan)
            } else {
                Ok(())
            }
        })
        .await;
        if let Ok(Err(error)) = result {
            eprintln!("Could not display a desktop notification: {error}");
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
        let (kind, sender_id, event_key, timestamp_ms, body) = match notification.event {
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
                if content
                    .as_ref()
                    .and_then(|content| content.get("m.relates_to"))
                    .and_then(|relation| relation.get("rel_type"))
                    .and_then(Value::as_str)
                    == Some("m.replace")
                {
                    return None;
                }
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
            timestamp_ms,
            kind,
            body,
            sdk_highlight,
            sdk_sound,
        })
    }
}

fn plan_notification(
    candidate: &Candidate,
    active: &AccountInfo,
    config: &Config,
    visible: Option<&VisibleRoom>,
    now_ms: u64,
) -> Option<PlannedNotification> {
    if candidate.account_id != active.id
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
    let summary = if highlight {
        format!("{room} · {account} · Highlight")
    } else {
        format!("{room} · {account}")
    };
    let body = match candidate.kind {
        Kind::Invite => format!("Invitation from {sender}"),
        Kind::Message if config.notifications.show_message_preview => {
            candidate.body.as_ref().map_or_else(
                || format!("{sender}: New message"),
                |body| format!("{sender}: {}", safe_text(body, MAX_TEXT_CHARS)),
            )
        }
        Kind::Message => format!("{sender}: New message"),
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

fn show_native(plan: PlannedNotification) -> Result<(), String> {
    let mut notification = Notification::new();
    notification.summary(&plan.summary).body(&plan.body);

    #[cfg(target_os = "macos")]
    {
        // The legacy macOS backend falls back to Finder when this ID is not
        // registered. Refuse to present under the wrong application identity.
        static APPLICATION_REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*APPLICATION_REGISTERED
            .get_or_init(|| notify_rust::set_application("io.github.brk275.morpheus").is_ok())
        {
            return Err("Morpheus app bundle is not registered with macOS".into());
        }
        if plan.sound {
            // The macOS backend reserves this exact name for the system sound.
            notification.sound_name("NSUserNotificationDefaultSoundName");
        }
    }
    #[cfg(target_os = "windows")]
    {
        notification.app_id("io.github.brk275.morpheus");
        if plan.sound {
            notification.sound_name("IM");
        }
    }
    #[cfg(target_os = "linux")]
    {
        notification
            .appname("Morpheus")
            .icon("io.github.brk275.morpheus");
        if plan.sound {
            notification.sound_name("message-new-instant");
        } else {
            notification.hint(notify_rust::Hint::SuppressSound(true));
        }
    }
    notification
        .show()
        .map(|_| ())
        .map_err(|error| error.to_string())
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
            timestamp_ms: Some(1_000_000),
            kind: Kind::Message,
            body: Some("The secret <code> is ready".into()),
            sdk_highlight: false,
            sdk_sound: true,
        }
    }

    #[test]
    fn active_account_visible_room_and_own_message_are_suppressed() {
        let active = account("alice", "@alice:example.org");
        let config = Config::default();
        let event = candidate();
        assert!(plan_notification(&event, &active, &config, None, 1_000_000).is_some());
        assert!(
            plan_notification(
                &event,
                &account("carol", "@carol:other.org"),
                &config,
                None,
                1_000_000
            )
            .is_none()
        );
        let visible = VisibleRoom {
            account_id: "alice".into(),
            room_id: "!room:example.org".into(),
        };
        assert!(plan_notification(&event, &active, &config, Some(&visible), 1_000_000).is_none());
        let mut own = event;
        own.sender_id = active.user_id.clone();
        assert!(plan_notification(&own, &active, &config, None, 1_000_000).is_none());
    }

    #[test]
    fn privacy_sound_config_and_stale_events_are_respected() {
        let active = account("alice", "@alice:example.org");
        let mut config = Config::default();
        let event = candidate();
        let plan = plan_notification(&event, &active, &config, None, 1_000_000).unwrap();
        assert!(!plan.body.contains("secret"));
        assert!(plan.sound);
        config.notifications.show_message_preview = true;
        config.notifications.sounds = false;
        config.notifications.highlight_words.push("secret".into());
        let plan = plan_notification(&event, &active, &config, None, 1_000_000).unwrap();
        assert!(plan.body.contains("secret ‹code›"));
        assert!(plan.summary.contains("Highlight"));
        assert!(!plan.sound);
        assert!(plan_notification(&event, &active, &config, None, 1_000_000 + 301_000).is_none());
        config
            .account_overrides
            .entry("alice".into())
            .or_default()
            .notifications_enabled = Some(false);
        assert!(plan_notification(&event, &active, &config, None, 1_000_000).is_none());
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
            1_000_000,
        )
        .unwrap();
        assert_eq!(plan.summary, "room name · @alice:example.org");
    }
}
