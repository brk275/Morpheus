//! Account-scoped room membership and power-level administration.
//! Permission checks and the final authorization decision belong to the
//! homeserver; this API keeps all mutations tied to the visible account.

use matrix_sdk::{
    RoomMemberships,
    ruma::{Int, events::room::power_levels::UserPowerLevel},
};
use serde::{Deserialize, Serialize};

use crate::{
    AccountManager, MatrixCoreError, Result, account_room, parse_user_id, selected_loaded_for,
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

    /// Update a member's exact Matrix power level. Matrix decides whether the
    /// selected user is allowed to make this change, including creator rules.
    /// Holding the account selection lock prevents a delayed action from
    /// writing under another account after a switch.
    pub async fn set_member_power_level_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        user_id: &str,
        level: i64,
    ) -> Result<()> {
        let user_id = parse_user_id(user_id)?;
        let level = Int::try_from(level).map_err(|error| {
            MatrixCoreError::InvalidPowerLevel(format!(
                "power level is outside Matrix's safe integer range: {error}"
            ))
        })?;
        let state = self.state.lock().await;
        selected_loaded_for(&state, expected_account_id)?;
        let room = account_room(&state, expected_account_id, room_id)?;
        room.update_power_levels(vec![(&user_id, level)])
            .await
            .map(|_| ())
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
    }
}
