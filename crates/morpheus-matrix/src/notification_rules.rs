//! Matrix push-rule notification modes and read markers for one account
//! selection.
//!
//! Room modes and account defaults are the account's server-side push rules,
//! read and written through the SDK's notification-settings API; nothing here
//! invents or caches server state. Every entry point is bound to the caller's
//! `AccountSelection`: the account must still own that exact selection when
//! the request starts and when its result is returned. A push-rule or receipt
//! request already handed to the homeserver cannot be recalled by a later
//! switch; the caller then only loses the result.

use std::time::Duration;

use matrix_sdk::{
    Client,
    notification_settings::{IsEncrypted, IsOneToOne, NotificationSettings, RoomNotificationMode},
    ruma::{RoomId, api::client::receipt::create_receipt::v3::ReceiptType},
};

use super::*;

/// Bound on one push-rule or receipt round trip.
const RULE_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on rooms described by one listing.
pub const MAX_NOTIFICATION_ROOMS: usize = 2_000;
/// Matrix keyword rules Morpheus adds at most, and their length in characters.
pub const MAX_NOTIFICATION_KEYWORDS: usize = 32;
pub const MAX_NOTIFICATION_KEYWORD_CHARS: usize = 64;

/// One room or account notification level, as the Matrix push rules define it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationMode {
    AllMessages,
    MentionsAndKeywords,
    Mute,
}

impl From<RoomNotificationMode> for NotificationMode {
    fn from(mode: RoomNotificationMode) -> Self {
        match mode {
            RoomNotificationMode::AllMessages => Self::AllMessages,
            RoomNotificationMode::MentionsAndKeywordsOnly => Self::MentionsAndKeywords,
            RoomNotificationMode::Mute => Self::Mute,
        }
    }
}

impl From<NotificationMode> for RoomNotificationMode {
    fn from(mode: NotificationMode) -> Self {
        match mode {
            NotificationMode::AllMessages => Self::AllMessages,
            NotificationMode::MentionsAndKeywords => Self::MentionsAndKeywordsOnly,
            NotificationMode::Mute => Self::Mute,
        }
    }
}

/// The notification rule of one joined room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomNotificationRule {
    pub room_id: String,
    /// Display name as the room list shows it; untrusted text.
    pub name: String,
    pub is_direct: bool,
    /// The room's own push rule, or `None` when it follows the account default.
    pub custom: Option<NotificationMode>,
    /// What applies now: the room's own rule, else the account default for
    /// this kind of room.
    pub effective: NotificationMode,
}

/// The account's default levels. Matrix keeps separate default rules for
/// encrypted and unencrypted rooms; Morpheus edits both together and reports
/// when they differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationDefaults {
    /// Rooms that do not have exactly two members.
    pub group: NotificationMode,
    /// Rooms with exactly two members.
    pub direct: NotificationMode,
    /// The encrypted and unencrypted rules differ; `group` is the encrypted one.
    pub group_mixed: bool,
    /// The encrypted and unencrypted rules differ; `direct` is the encrypted one.
    pub direct_mixed: bool,
}

/// Unread counts the SDK computed from read receipts for one joined room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomUnreadCounts {
    pub room_id: String,
    pub notifications: u64,
    pub mentions: u64,
}

impl AccountManager {
    fn owns_rule_selection(&self, selection: &AccountSelection) -> bool {
        selection.is_current()
            && self
                .account_selection(selection.account_id())
                .is_some_and(|current| selection.same_epoch(&current))
    }

    /// The selected account's client while `selection` still owns it. The
    /// manager lock is released before any homeserver request.
    async fn rule_client(&self, selection: &AccountSelection) -> Result<Client> {
        if !self.owns_rule_selection(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let state = selection
            .guard(self.state.lock())
            .await
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        if !self.owns_rule_selection(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        super::account_access::ensure_eligible(&state, selection.account_id())?;
        Ok(selected_loaded_for(&state, selection.account_id())?
            .client
            .clone())
    }

    async fn rule_loaded(&self, selection: &AccountSelection) -> Result<Arc<LoadedAccount>> {
        if !self.owns_rule_selection(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let state = selection
            .guard(self.state.lock())
            .await
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        if !self.owns_rule_selection(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        super::account_access::ensure_eligible(&state, selection.account_id())?;
        Ok(selected_loaded_for(&state, selection.account_id())?.clone())
    }

    /// Run one bounded request under `selection`; a revoked selection or a
    /// timeout ends it and its result is never returned.
    async fn under_rule_selection<T>(
        &self,
        selection: &AccountSelection,
        request: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let result = selection
            .guard(tokio::time::timeout(RULE_TIMEOUT, request))
            .await
            .ok_or(MatrixCoreError::SelectedAccountChanged)?
            .map_err(|_| MatrixCoreError::Sdk("the homeserver did not answer in time".into()))??;
        if !self.owns_rule_selection(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(result)
    }

    /// The notification rule of every joined room (Spaces excluded), sorted
    /// like the room list and bounded by `MAX_NOTIFICATION_ROOMS`. Reads the
    /// push rules the SDK holds from sync; no request is sent.
    pub async fn room_notification_rules_selected(
        &self,
        selection: &AccountSelection,
    ) -> Result<Vec<RoomNotificationRule>> {
        let client = self.rule_client(selection).await?;
        self.under_rule_selection(selection, async move {
            let settings = client.notification_settings().await;
            let mut rules = Vec::new();
            for room in client.joined_rooms() {
                if room.is_space() {
                    continue;
                }
                if rules.len() >= MAX_NOTIFICATION_ROOMS {
                    break;
                }
                rules.push(describe_room(&settings, &room).await);
            }
            rules.sort_by(|a: &RoomNotificationRule, b| {
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.room_id.cmp(&b.room_id))
            });
            Ok(rules)
        })
        .await
    }

    /// Give one joined room its own push rule, or with `None` remove its own
    /// rules so it follows the account default. Returns the rule the SDK
    /// reports after the homeserver accepted the change.
    pub async fn set_room_notification_mode_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        mode: Option<NotificationMode>,
    ) -> Result<RoomNotificationRule> {
        let id =
            RoomId::parse(room_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        let client = self.rule_client(selection).await?;
        let room = client
            .get_room(&id)
            .filter(|room| matches!(room.state(), RoomState::Joined) && !room.is_space())
            .ok_or_else(|| MatrixCoreError::RoomNotFound(room_id.into()))?;
        self.under_rule_selection(selection, async move {
            let settings = client.notification_settings().await;
            match mode {
                Some(mode) => settings
                    .set_room_notification_mode(room.room_id(), mode.into())
                    .await
                    .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?,
                None => settings
                    .delete_user_defined_room_rules(room.room_id())
                    .await
                    .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?,
            }
            Ok(describe_room(&settings, &room).await)
        })
        .await
    }

    /// The account's default levels for group rooms and two-person rooms.
    pub async fn notification_defaults_selected(
        &self,
        selection: &AccountSelection,
    ) -> Result<NotificationDefaults> {
        let client = self.rule_client(selection).await?;
        self.under_rule_selection(selection, async move {
            let settings = client.notification_settings().await;
            Ok(read_defaults(&settings).await)
        })
        .await
    }

    /// Set the default level for group rooms (`direct == false`) or rooms with
    /// exactly two members, in encrypted and unencrypted rooms alike. Matrix
    /// defaults are either all messages or mentions and keywords; a muted
    /// default is not representable and is refused.
    pub async fn set_notification_default_selected(
        &self,
        selection: &AccountSelection,
        direct: bool,
        mode: NotificationMode,
    ) -> Result<NotificationDefaults> {
        if mode == NotificationMode::Mute {
            return Err(MatrixCoreError::Sdk(
                "a default notification level cannot mute rooms".into(),
            ));
        }
        let client = self.rule_client(selection).await?;
        self.under_rule_selection(selection, async move {
            let settings = client.notification_settings().await;
            for encrypted in [true, false] {
                settings
                    .set_default_room_notification_mode(
                        IsEncrypted::from(encrypted),
                        IsOneToOne::from(direct),
                        mode.into(),
                    )
                    .await
                    .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
            }
            Ok(read_defaults(&settings).await)
        })
        .await
    }

    /// The account's enabled Matrix keyword rules, sorted without regard to
    /// case. They notify in every room that is not muted, including rooms set
    /// to mentions and keywords, on all of the account's devices.
    pub async fn notification_keywords_selected(
        &self,
        selection: &AccountSelection,
    ) -> Result<Vec<String>> {
        let client = self.rule_client(selection).await?;
        self.under_rule_selection(selection, async move {
            let settings = client.notification_settings().await;
            Ok(sorted_keywords(&settings).await)
        })
        .await
    }

    /// Add (or re-enable) one plain keyword rule. Glob characters, control or
    /// direction characters, blank or overlong keywords and more than
    /// `MAX_NOTIFICATION_KEYWORDS` rules are refused before any request.
    pub async fn add_notification_keyword_selected(
        &self,
        selection: &AccountSelection,
        keyword: &str,
    ) -> Result<Vec<String>> {
        if !valid_keyword(keyword) {
            return Err(MatrixCoreError::Sdk("invalid notification keyword".into()));
        }
        let keyword = keyword.to_owned();
        let client = self.rule_client(selection).await?;
        self.under_rule_selection(selection, async move {
            let settings = client.notification_settings().await;
            let existing = sorted_keywords(&settings).await;
            if existing.iter().any(|known| known == &keyword) {
                return Ok(existing);
            }
            if existing.len() >= MAX_NOTIFICATION_KEYWORDS {
                return Err(MatrixCoreError::Sdk("too many notification keywords".into()));
            }
            settings
                .add_keyword(keyword)
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
            Ok(sorted_keywords(&settings).await)
        })
        .await
    }

    /// Remove every rule for one keyword.
    pub async fn remove_notification_keyword_selected(
        &self,
        selection: &AccountSelection,
        keyword: &str,
    ) -> Result<Vec<String>> {
        let keyword = keyword.to_owned();
        let client = self.rule_client(selection).await?;
        self.under_rule_selection(selection, async move {
            let settings = client.notification_settings().await;
            settings
                .remove_keyword(&keyword)
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
            Ok(sorted_keywords(&settings).await)
        })
        .await
    }

    /// Mark the room's live timeline read up to its latest event with a
    /// private read receipt, which clears unread counts on this and the
    /// account's other devices without showing other members a read marker.
    /// Returns whether a receipt was sent; an already read room sends none.
    pub async fn mark_room_read_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> Result<bool> {
        let loaded = self.rule_loaded(selection).await?;
        self.under_rule_selection(selection, async move {
            let timeline = loaded.timeline(room_id).await?;
            timeline
                .mark_as_read(ReceiptType::ReadPrivate)
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
        })
        .await
    }

    /// `mark_room_read_selected` for one thread's timeline.
    pub async fn mark_thread_read_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        root_event_id: &str,
    ) -> Result<bool> {
        let loaded = self.rule_loaded(selection).await?;
        self.under_rule_selection(selection, async move {
            let timeline = loaded.thread_timeline(room_id, root_event_id).await?;
            timeline
                .mark_as_read(ReceiptType::ReadPrivate)
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
        })
        .await
    }

    /// Current SDK unread counts for at most 64 joined rooms. Unknown,
    /// left or malformed room IDs are omitted.
    pub async fn room_unread_counts_selected(
        &self,
        selection: &AccountSelection,
        room_ids: &[String],
    ) -> Result<Vec<RoomUnreadCounts>> {
        let client = self.rule_client(selection).await?;
        let counts = room_ids
            .iter()
            .take(64)
            .filter_map(|room_id| {
                let id = RoomId::parse(room_id).ok()?;
                let room = client.get_room(&id)?;
                matches!(room.state(), RoomState::Joined).then(|| RoomUnreadCounts {
                    room_id: room_id.clone(),
                    notifications: room.num_unread_notifications(),
                    mentions: room.num_unread_mentions(),
                })
            })
            .collect();
        if !self.owns_rule_selection(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(counts)
    }
}

/// A keyword is matched as a push-rule glob, so only plain text is accepted.
fn valid_keyword(keyword: &str) -> bool {
    let length = keyword.chars().count();
    length > 0
        && length <= MAX_NOTIFICATION_KEYWORD_CHARS
        && keyword.trim() == keyword
        && !keyword.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '*' | '?'
                        | '['
                        | ']'
                        | '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{feff}'
                )
        })
}

async fn sorted_keywords(settings: &NotificationSettings) -> Vec<String> {
    let mut keywords = settings
        .enabled_keywords()
        .await
        .into_iter()
        .collect::<Vec<String>>();
    keywords.sort_by_key(|keyword| keyword.to_lowercase());
    keywords
}

async fn describe_room(settings: &NotificationSettings, room: &Room) -> RoomNotificationRule {
    let room_id = room.room_id().to_string();
    let name = room
        .display_name()
        .await
        .map(|name| name.to_string())
        .unwrap_or_else(|_| room_id.clone());
    let custom = settings
        .get_user_defined_room_notification_mode(room.room_id())
        .await
        .map(NotificationMode::from);
    let effective = match custom {
        Some(mode) => mode,
        // The cached encryption state; an unknown state reads as unencrypted
        // rather than sending a state request per listed room.
        None => settings
            .get_default_room_notification_mode(
                IsEncrypted::from(room.encryption_state().is_encrypted()),
                IsOneToOne::from(room.active_members_count() == 2),
            )
            .await
            .into(),
    };
    RoomNotificationRule {
        room_id,
        name,
        is_direct: room.is_direct().await.unwrap_or(false),
        custom,
        effective,
    }
}

async fn read_defaults(settings: &NotificationSettings) -> NotificationDefaults {
    let mut levels = [NotificationMode::AllMessages; 4];
    for (index, (encrypted, direct)) in [(true, false), (false, false), (true, true), (false, true)]
        .into_iter()
        .enumerate()
    {
        levels[index] = settings
            .get_default_room_notification_mode(
                IsEncrypted::from(encrypted),
                IsOneToOne::from(direct),
            )
            .await
            .into();
    }
    NotificationDefaults {
        group: levels[0],
        direct: levels[2],
        group_mixed: levels[0] != levels[1],
        direct_mixed: levels[2] != levels[3],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_through_the_sdk_levels() {
        for mode in [
            NotificationMode::AllMessages,
            NotificationMode::MentionsAndKeywords,
            NotificationMode::Mute,
        ] {
            assert_eq!(NotificationMode::from(RoomNotificationMode::from(mode)), mode);
        }
    }

    #[test]
    fn only_plain_bounded_keywords_become_push_rules() {
        assert!(valid_keyword("release"));
        assert!(valid_keyword("on call"));
        for keyword in [
            "",
            " release",
            "release ",
            "re*lease",
            "release?",
            "[release]",
            "re\nlease",
            "re\u{202e}lease",
        ] {
            assert!(!valid_keyword(keyword), "{keyword:?}");
        }
        assert!(valid_keyword(&"a".repeat(MAX_NOTIFICATION_KEYWORD_CHARS)));
        assert!(!valid_keyword(&"a".repeat(MAX_NOTIFICATION_KEYWORD_CHARS + 1)));
    }
}
