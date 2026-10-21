//! Account-scoped room membership and power-level administration.
//! Permission checks and the final authorization decision belong to the
//! homeserver; this API keeps all mutations tied to the visible account.
//! The `_for` calls run through the bounded, at-most-once paths: member
//! actions, power levels and redactions through `moderation`, the room name
//! and topic through a fresh `room_settings` session.

use matrix_sdk::{RoomMemberships, ruma::events::room::power_levels::UserPowerLevel};
use serde::{Deserialize, Serialize};

use crate::{
    AccountManager, MatrixCoreError, MemberAction, ModerationCancellation, ModerationError,
    ModerationScope, Result, RoomSettingsCancellation, RoomSettingsError, account_room,
    parse_user_id, selected_loaded_for,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomMemberInfo {
    pub user_id: String,
    pub display_name: Option<String>,
    pub avatar_mxc: Option<String>,
    pub membership: String,
    /// `None` denotes an immutable room creator in room versions where
    /// creators have infinite power; otherwise this is the exact Matrix level.
    pub power_level: Option<i64>,
    pub is_account_user: bool,
}

impl AccountManager {
    /// Invite a user without allowing a delayed UI action to use a different
    /// account after an account switch.
    pub async fn invite_user_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        user_id: &str,
    ) -> Result<()> {
        self.member_action_for(expected_account_id, room_id, user_id, MemberAction::Invite)
            .await
    }

    /// Kick a member from the selected account's room. Matrix enforces the
    /// caller's power level and any creator protections.
    pub async fn kick_user_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let reason = reason.map(str::to_owned);
        let action = MemberAction::Kick { reason };
        self.member_action_for(expected_account_id, room_id, user_id, action)
            .await
    }

    pub async fn ban_user_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let reason = reason.map(str::to_owned);
        let action = MemberAction::Ban { reason };
        self.member_action_for(expected_account_id, room_id, user_id, action)
            .await
    }

    pub async fn unban_user_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let reason = reason.map(str::to_owned);
        let action = MemberAction::Unban { reason };
        self.member_action_for(expected_account_id, room_id, user_id, action)
            .await
    }

    async fn member_action_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        user_id: &str,
        action: MemberAction,
    ) -> Result<()> {
        parse_user_id(user_id)?;
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        let cancellation = ModerationCancellation::new();
        let scope = ModerationScope {
            selection: &selection,
            cancellation: &cancellation,
            admission: &|| true,
        };
        self.moderate_member_selected(&scope, room_id, user_id, action)
            .await
            .map(|_| ())
            .map_err(moderation_failure)
    }

    /// Rename through a fresh room-settings session: its permission and
    /// baseline read, then one bounded write.
    pub async fn rename_room_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        name: &str,
    ) -> Result<()> {
        let session = self.settings_for(expected_account_id, room_id).await?;
        session
            .set_name(name)
            .await
            .map(|_| ())
            .map_err(settings_failure)
    }

    /// Set the topic through a fresh room-settings session, like the name.
    pub async fn set_room_topic_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        topic: &str,
    ) -> Result<()> {
        let session = self.settings_for(expected_account_id, room_id).await?;
        session
            .set_topic(topic)
            .await
            .map(|_| ())
            .map_err(settings_failure)
    }

    async fn settings_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
    ) -> Result<crate::RoomSettingsSession> {
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        let cancellation = RoomSettingsCancellation::new();
        self.open_room_settings_for(&selection, room_id, cancellation)
            .await
            .map_err(settings_failure)
    }

    pub async fn redact_message_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        let cancellation = ModerationCancellation::new();
        let scope = ModerationScope {
            selection: &selection,
            cancellation: &cancellation,
            admission: &|| true,
        };
        self.redact_event_selected(&scope, room_id, event_id, reason)
            .await
            .map(|_| ())
            .map_err(moderation_failure)
    }

    /// Fetch the joined, invited, and banned membership list for a room.
    /// Unlike a cached-only snapshot, this fetches the full list when the
    /// homeserver provided lazy-loaded members. A result is discarded if the
    /// user changed accounts while the request was in flight.
    pub async fn room_members_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
    ) -> Result<Vec<RoomMemberInfo>> {
        let room = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?;
            account_room(&state, expected_account_id, room_id)?
        };
        let members = room
            .members(RoomMemberships::JOIN | RoomMemberships::INVITE | RoomMemberships::BAN)
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
        let mut result = members
            .into_iter()
            .map(|member| RoomMemberInfo {
                user_id: member.user_id().to_string(),
                display_name: member.display_name().map(str::to_owned),
                avatar_mxc: member.avatar_url().map(ToString::to_string),
                membership: member.membership().as_str().to_owned(),
                power_level: match member.power_level() {
                    UserPowerLevel::Infinite => None,
                    UserPowerLevel::Int(level) => Some(level.into()),
                    _ => None,
                },
                is_account_user: member.is_account_user(),
            })
            .collect::<Vec<_>>();
        result.sort_by(|a, b| {
            a.power_level
                .is_some()
                .cmp(&b.power_level.is_some())
                .then_with(|| b.power_level.cmp(&a.power_level))
                .then_with(|| a.display_name.cmp(&b.display_name))
                .then_with(|| a.user_id.cmp(&b.user_id))
        });
        {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?;
        }
        Ok(result)
    }

    /// Update a member's exact Matrix power level through the bounded,
    /// lossless power-level write (`moderation`). Matrix decides whether the
    /// selected user is allowed to make this change, including creator rules;
    /// Morpheus refuses first what the fresh power levels already forbid.
    pub async fn set_member_power_level_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        user_id: &str,
        level: i64,
    ) -> Result<()> {
        parse_user_id(user_id)?;
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        let cancellation = ModerationCancellation::new();
        let scope = ModerationScope {
            selection: &selection,
            cancellation: &cancellation,
            admission: &|| true,
        };
        self.set_member_power_level_selected(&scope, room_id, user_id, level)
            .await
            .map(|_| ())
            .map_err(moderation_failure)
    }
}

/// The fixed text of a moderation failure; a stale selection keeps its own
/// error so callers can tell an account switch apart.
fn moderation_failure(error: ModerationError) -> MatrixCoreError {
    match error {
        ModerationError::AccountChanged | ModerationError::Cancelled => {
            MatrixCoreError::SelectedAccountChanged
        }
        ModerationError::InvalidPowerLevel => MatrixCoreError::InvalidPowerLevel(error.to_string()),
        error => MatrixCoreError::Sdk(error.to_string()),
    }
}

fn settings_failure(error: RoomSettingsError) -> MatrixCoreError {
    match error {
        RoomSettingsError::AccountChanged | RoomSettingsError::Cancelled => {
            MatrixCoreError::SelectedAccountChanged
        }
        error => MatrixCoreError::Sdk(error.to_string()),
    }
}
