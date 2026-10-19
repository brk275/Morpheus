//! Pure rules for native room administration: bounded roster search, role
//! labels, deliberate confirmation and actionable messages. The UI adapter in
//! `ui.rs` holds the account selection, the exact room and a per-panel
//! generation across every asynchronous result.

pub(crate) mod ui;

pub(crate) use ui::clear_presentation;

use morpheus_matrix::{
    AccessChange, AccessSettings, AdminAction, AdminMember, AdminMembership, AdminOutcome,
    AdminRequest, HistoryChoice, MAX_ADMIN_REASON_BYTES, MemberExpectation, MemberPower,
    MembershipCounts, ObservedMembership, ObservedTarget, PERMISSION_KEYS, PermissionKey,
    PermissionSetting, PowerThresholds, RoomAdminError, RoomAdminSnapshot,
};

/// Rows shown at once. Search narrows a larger roster; the count says so.
pub(crate) const MAX_VISIBLE_ROWS: usize = 200;
const MAX_QUERY_CHARS: usize = 128;
const MAX_NAME_CHARS: usize = 100;
const MAX_USER_ID_BYTES: usize = 255;
/// Matrix's canonical JSON integer range.
const MAX_LEVEL: i64 = 9_007_199_254_740_991;

const BAD_USER_ID: &str = "Enter a full Matrix user ID, like @name:example.org.";
const LONG_REASON: &str = "Keep the reason to one line of at most 512 bytes.";
const CREATOR: &str = "Room creators cannot be removed and their level cannot change.";
const ASK_HIGHER: &str = "Ask someone with a higher level.";
const UNKNOWN_RULES: &str =
    "Morpheus cannot read this room's permission rules, so member changes are unavailable.";
const DENY_BODY: &str =
    "They will not join. They can ask again unless the room's rules prevent it.";
const KICK_BODY: &str =
    "They leave the room now and can rejoin if the room's access rules allow it.";
const BAN_BODY: &str = "They leave the room and cannot rejoin until someone unbans them.";
const UNBAN_BODY: &str =
    "They can join again if the room's access rules allow it. This does not invite them.";
const SELF_DEMOTION: &str =
    "You cannot raise it again yourself; someone with a higher level would have to.";
const DEMOTION: &str = "They lose permissions that need a higher level.";
const IRREVERSIBLE: &str = "You will not be able to lower it again.";
const UNREAD: &str = "Reading it back failed; refresh to check the current state.";
const CLOSED: &str = "The member list closed before the change finished. Reopen the room.";
const UNCONFIRMED: &str =
    "The homeserver did not confirm the change. It may still apply; check the refreshed roster.";
const UNREADABLE: &str = "This room returned roles Morpheus cannot read safely. Nothing was sent.";
const REFUSED: &str =
    "Your permissions or their role may have changed; review the refreshed roster.";
const REVIEW: &str = "Review the refreshed roster, then try again.";
pub(crate) const NO_ROUTE: &str =
    "The servers that know this room are unknown. Join it first, or use its address or a link.";
pub(crate) const UNSUPPORTED_CONTENT: &str =
    "This entry uses content Morpheus cannot change without losing information.";
const REFUSED_PERMISSION: &str = "Your permissions may have changed; review them again.";
const OWN_DEFAULT: &str = "Your own level follows the default, so it changes too.";
const CUSTOM_ACCESS: &str = "This room uses a setting Morpheus does not edit.";
const CUSTOM_HISTORY: &str = "Custom history setting";
const HISTORY_BODY: &str = "Messages already sent keep the visibility they were sent with.";
const GUESTS_ALLOW: &str = "Guest accounts can join whenever the room's join rules let them in.";
const GUESTS_FORBID: &str = "Guest accounts can no longer join. Guests already here stay.";
const SETTING_CHANGED: &str = "This setting changed on the server. Review it again.";

pub(crate) const KICK: i32 = 1;
pub(crate) const BAN: i32 = 2;
pub(crate) const UNBAN: i32 = 3;
pub(crate) const SET_LEVEL: i32 = 4;
pub(crate) const APPROVE: i32 = 5;
pub(crate) const DENY: i32 = 6;

/// Access rows in the permissions view: history visibility and guest access.
pub(crate) const ACCESS_HISTORY: i32 = 1;
pub(crate) const ACCESS_GUESTS: i32 = 2;
/// History options in the panel's order, narrowest first. The Slint option
/// labels follow the same order.
pub(crate) const HISTORY_CHOICES: [HistoryChoice; 4] = [
    HistoryChoice::Joined,
    HistoryChoice::Invited,
    HistoryChoice::Shared,
    HistoryChoice::WorldReadable,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Filter {
    All,
    Joined,
    Invited,
    Requests,
    Banned,
}

impl Filter {
    pub(crate) fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Joined,
            2 => Self::Invited,
            3 => Self::Requests,
            4 => Self::Banned,
            _ => Self::All,
        }
    }
    fn admits(self, membership: AdminMembership) -> bool {
        match self {
            Self::All => true,
            Self::Joined => membership == AdminMembership::Join,
            Self::Invited => membership == AdminMembership::Invite,
            Self::Requests => membership == AdminMembership::Knock,
            Self::Banned => membership == AdminMembership::Ban,
        }
    }
}

/// Members matching a case-insensitive search of names and Matrix IDs, in
/// snapshot order and at most `MAX_VISIBLE_ROWS`, with the full match count.
pub(crate) fn visible<'a>(
    snapshot: &'a RoomAdminSnapshot,
    query: &str,
    filter: Filter,
) -> (Vec<&'a AdminMember>, usize) {
    let needle = query
        .trim()
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .to_lowercase();
    let mut shown = Vec::new();
    let mut total = 0;
    for member in &snapshot.members {
        if !filter.admits(member.membership) || !matches_query(member, &needle) {
            continue;
        }
        total += 1;
        if shown.len() < MAX_VISIBLE_ROWS {
            shown.push(member);
        }
    }
    (shown, total)
}

fn matches_query(member: &AdminMember, needle: &str) -> bool {
    if needle.is_empty() || member.user_id.to_lowercase().contains(needle) {
        return true;
    }
    let name = member.display_name.as_deref().unwrap_or_default();
    name.to_lowercase().contains(needle)
}

/// Sanitized display name, falling back to the user ID's localpart.
pub(crate) fn display_name(member: &AdminMember) -> String {
    member
        .display_name
        .as_deref()
        .map(|name| crate::directory_plain_text(name, MAX_NAME_CHARS))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            let localpart = crate::live::display_user(&member.user_id);
            crate::directory_plain_text(&localpart, MAX_NAME_CHARS)
        })
}

pub(crate) fn role(power: MemberPower, thresholds: Option<&PowerThresholds>) -> &'static str {
    let default = thresholds.map_or(0, |levels| levels.users_default);
    match power {
        MemberPower::Creator => "Creator",
        MemberPower::Level(level) if level >= 100 => "Administrator",
        MemberPower::Level(level) if level >= 50 => "Moderator",
        MemberPower::Level(level) if level == default => "Member",
        MemberPower::Level(level) if level < default => "Restricted",
        MemberPower::Level(_) => "Custom",
    }
}

pub(crate) fn power_text(power: MemberPower) -> String {
    match power {
        MemberPower::Creator => "unlimited power".to_owned(),
        MemberPower::Level(level) => format!("level {level}"),
    }
}

fn membership_text(membership: AdminMembership) -> &'static str {
    match membership {
        AdminMembership::Join => "Joined",
        AdminMembership::Invite => "Invited",
        AdminMembership::Knock => "Asked to join",
        AdminMembership::Ban => "Banned",
    }
}

/// The secondary line of a roster row: role, level and non-joined state.
pub(crate) fn row_detail(member: &AdminMember, thresholds: Option<&PowerThresholds>) -> String {
    let mut detail = format!(
        "{} · {}",
        role(member.power, thresholds),
        power_text(member.power)
    );
    if member.membership != AdminMembership::Join {
        detail.push_str(" · ");
        detail.push_str(membership_text(member.membership));
    }
    detail
}

/// The selected member's state, including whether the level is inherited.
pub(crate) fn status_text(member: &AdminMember, thresholds: Option<&PowerThresholds>) -> String {
    let source = if member.power == MemberPower::Creator {
        "room creator"
    } else if member.explicit_level {
        "set for this member"
    } else {
        "the room's default"
    };
    format!(
        "{} · {} · {} ({source})",
        membership_text(member.membership),
        role(member.power, thresholds),
        power_text(member.power)
    )
}

pub(crate) fn summary(counts: MembershipCounts) -> String {
    let mut parts = vec![format!("{} joined", counts.joined)];
    if counts.invited > 0 {
        parts.push(format!("{} invited", counts.invited));
    }
    if counts.knocking > 0 {
        let noun = if counts.knocking == 1 {
            "request"
        } else {
            "requests"
        };
        parts.push(format!("{} {noun}", counts.knocking));
    }
    if counts.banned > 0 {
        parts.push(format!("{} banned", counts.banned));
    }
    parts.join(" · ")
}

pub(crate) fn own_role_text(snapshot: &RoomAdminSnapshot) -> String {
    format!(
        "You: {} · {}",
        role(snapshot.own_power, snapshot.thresholds.as_ref()),
        power_text(snapshot.own_power)
    )
}

pub(crate) fn shown_text(shown: usize, total: usize, truncated: bool) -> String {
    let mut text = String::new();
    if total > shown {
        text = format!("Showing {shown} of {total}. Search to narrow the list.");
    }
    if truncated {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str("This room has more members than Morpheus lists at once.");
    }
    text
}

pub(crate) fn permission_label(key: PermissionKey) -> &'static str {
    match key {
        PermissionKey::UsersDefault => "Default member level",
        PermissionKey::EventsDefault => "Send messages",
        PermissionKey::Invite => "Invite people",
        PermissionKey::Kick => "Kick members",
        PermissionKey::Ban => "Ban members",
        PermissionKey::Redact => "Remove others' messages",
        PermissionKey::RoomName => "Change the room name",
        PermissionKey::RoomTopic => "Change the topic",
        PermissionKey::RoomAvatar => "Change the room avatar",
        PermissionKey::JoinRules => "Change who can join",
        PermissionKey::PowerLevels => "Change roles and permissions",
        PermissionKey::RoomNotification => "Notify everyone with @room",
    }
}

pub(crate) fn permission_description(key: PermissionKey) -> &'static str {
    match key {
        PermissionKey::UsersDefault => "The level of members who have no level set for them.",
        PermissionKey::EventsDefault => "Messages, reactions and events without their own rule.",
        PermissionKey::Invite => "Invite people and approve requests to join.",
        PermissionKey::Kick => "Remove members or deny requests. Also needs a level above theirs.",
        PermissionKey::Ban => "Ban members. Unbanning also needs the kick level.",
        PermissionKey::Redact => "Remove messages sent by other members.",
        PermissionKey::RoomName => "Edit the room's name.",
        PermissionKey::RoomTopic => "Edit the room's topic.",
        PermissionKey::RoomAvatar => "Change or remove the room's avatar.",
        PermissionKey::JoinRules => "Choose whether the room is public, invite-only or knockable.",
        PermissionKey::PowerLevels => "Assign levels and change these permissions.",
        PermissionKey::RoomNotification => "Mention everyone in the room at once.",
    }
}

/// Who currently qualifies, in role words where the level matches a role.
pub(crate) fn permission_summary(
    setting: PermissionSetting,
    thresholds: Option<&PowerThresholds>,
) -> String {
    let level = setting.level;
    let default = thresholds.map_or(0, |levels| levels.users_default);
    if setting.key == PermissionKey::UsersDefault {
        return format!("Members start at level {level}");
    }
    if level <= default {
        return format!("Everyone · level {level}");
    }
    match role(MemberPower::Level(level), thresholds) {
        "Administrator" => format!("Administrators · level {level}"),
        "Moderator" => format!("Moderators and above · level {level}"),
        _ => format!("Level {level} and above"),
    }
}

/// Why the selected account cannot change a threshold right now.
pub(crate) fn permission_restriction(key: PermissionKey, snapshot: &RoomAdminSnapshot) -> String {
    let own = power_text(snapshot.own_power);
    let Some(levels) = snapshot.thresholds else {
        return UNKNOWN_RULES.to_owned();
    };
    let needed = levels.change_power_levels;
    if snapshot.own_power < MemberPower::Level(needed) {
        return format!("Changing permissions needs level {needed}; you have {own}. {ASK_HIGHER}");
    }
    let current = snapshot.permission(key).map_or(needed, |row| row.level);
    format!("Only someone with level {current} or higher can change this; you have {own}.")
}

pub(crate) fn history_text(choice: HistoryChoice) -> &'static str {
    match choice {
        HistoryChoice::Joined => "Members, from when they joined",
        HistoryChoice::Invited => "Members, from when they were invited",
        HistoryChoice::Shared => "Members, including earlier messages",
        HistoryChoice::WorldReadable => "Anyone, without joining",
    }
}

/// The choice's index in `HISTORY_CHOICES`; a higher rank shows more.
fn history_rank(choice: HistoryChoice) -> i32 {
    match choice {
        HistoryChoice::Joined => 0,
        HistoryChoice::Invited => 1,
        HistoryChoice::Shared => 2,
        HistoryChoice::WorldReadable => 3,
    }
}

/// Display state of one access row in the permissions view.
pub(crate) struct AccessRow {
    pub(crate) kind: i32,
    /// History: index in `HISTORY_CHOICES`. Guests: 1 allowed, 0 not. -1 custom.
    pub(crate) choice: i32,
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) editable: bool,
    pub(crate) restriction: String,
}

/// Why an access row is read-only, or empty when it can be changed.
fn access_lock(kind: i32, access: AccessSettings, snapshot: &RoomAdminSnapshot) -> String {
    let (known, editable, level) = match kind {
        ACCESS_HISTORY => {
            let known = access.history.is_some();
            (known, access.history_editable, access.history_level)
        }
        _ => {
            let known = access.guests_can_join.is_some();
            (known, access.guests_editable, access.guests_level)
        }
    };
    if editable {
        String::new()
    } else if !known {
        CUSTOM_ACCESS.to_owned()
    } else {
        access_restriction(level, snapshot)
    }
}

pub(crate) fn access_rows(snapshot: &RoomAdminSnapshot) -> Vec<AccessRow> {
    let Some(access) = snapshot.access else {
        return Vec::new();
    };
    let guests = match access.guests_can_join {
        Some(true) => "Guests can join",
        Some(false) => "Guests cannot join",
        None => "Custom guest setting",
    };
    vec![
        AccessRow {
            kind: ACCESS_HISTORY,
            choice: access.history.map_or(-1, history_rank),
            label: "History visibility",
            description: "Who can read new messages in this room.",
            summary: access.history.map_or(CUSTOM_HISTORY, history_text),
            editable: access.history_editable,
            restriction: access_lock(ACCESS_HISTORY, access, snapshot),
        },
        AccessRow {
            kind: ACCESS_GUESTS,
            choice: access.guests_can_join.map_or(-1, i32::from),
            label: "Guest access",
            description: "Whether guest accounts can join when the join rules let them.",
            summary: guests,
            editable: access.guests_editable,
            restriction: access_lock(ACCESS_GUESTS, access, snapshot),
        },
    ]
}

fn access_restriction(level: i64, snapshot: &RoomAdminSnapshot) -> String {
    let own = power_text(snapshot.own_power);
    format!("Changing this needs level {level}; you have {own}. {ASK_HIGHER}")
}

/// The reviewed value of an access change is still the room's value.
fn access_current(change: AccessChange, access: AccessSettings) -> bool {
    match change {
        AccessChange::History { from, .. } => access.history == Some(from),
        AccessChange::Guests { allow } => access.guests_can_join == Some(!allow),
    }
}

/// Change history visibility (`ACCESS_HISTORY`, choice indexes
/// `HISTORY_CHOICES`) or guest access (`ACCESS_GUESTS`, choice 1 allows).
pub(crate) fn access_intent(
    kind: i32,
    choice: i32,
    snapshot: &RoomAdminSnapshot,
) -> Result<Intent, String> {
    let Some(access) = snapshot.access else {
        return Err(UNKNOWN_RULES.to_owned());
    };
    let (change, name) = match kind {
        ACCESS_HISTORY => {
            let from = access.history.ok_or_else(|| CUSTOM_ACCESS.to_owned())?;
            if !access.history_editable {
                return Err(access_restriction(access.history_level, snapshot));
            }
            let to = usize::try_from(choice)
                .ok()
                .and_then(|index| HISTORY_CHOICES.get(index).copied())
                .ok_or_else(|| "Choose who can read new messages.".to_owned())?;
            if to == from {
                return Err("The room already uses that history setting.".to_owned());
            }
            (AccessChange::History { from, to }, "History visibility")
        }
        ACCESS_GUESTS => {
            let current = access.guests_can_join;
            let current = current.ok_or_else(|| CUSTOM_ACCESS.to_owned())?;
            if !access.guests_editable {
                return Err(access_restriction(access.guests_level, snapshot));
            }
            let allow = choice == 1;
            if allow == current {
                return Err("The room already uses that guest setting.".to_owned());
            }
            (AccessChange::Guests { allow }, "Guest access")
        }
        _ => return Err(UNKNOWN_RULES.to_owned()),
    };
    Ok(Intent {
        action: AdminAction::SetAccess(change),
        user_id: String::new(),
        name: name.to_owned(),
        expected: MemberExpectation {
            membership: None,
            power: None,
        },
        reason: None,
        knock: false,
    })
}

/// Change one threshold, identified by its index in `PERMISSION_KEYS`.
pub(crate) fn permission_intent(
    index: i32,
    level_input: &str,
    snapshot: &RoomAdminSnapshot,
) -> Result<Intent, String> {
    let key = usize::try_from(index)
        .ok()
        .and_then(|index| PERMISSION_KEYS.get(index).copied());
    let Some(setting) = key.and_then(|key| snapshot.permission(key)) else {
        return Err(UNKNOWN_RULES.to_owned());
    };
    if !setting.editable {
        return Err(permission_restriction(setting.key, snapshot));
    }
    let label = permission_label(setting.key);
    let level = parse_level(level_input)?;
    if level == setting.level {
        return Err(format!("{label} already needs level {level}."));
    }
    if MemberPower::Level(level) > snapshot.own_power {
        let own = power_text(snapshot.own_power);
        return Err(format!("You can set permissions up to your own level, {own}."));
    }
    Ok(Intent {
        action: AdminAction::SetPermission(setting.key, level),
        user_id: String::new(),
        name: label.to_owned(),
        expected: MemberExpectation {
            membership: None,
            power: Some(MemberPower::Level(setting.level)),
        },
        reason: None,
        knock: false,
    })
}

/// The operator's decision, captured from the row they reviewed. The
/// session refuses it when the server's state no longer matches `expected`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Intent {
    pub(crate) action: AdminAction,
    pub(crate) user_id: String,
    pub(crate) name: String,
    pub(crate) expected: MemberExpectation,
    pub(crate) reason: Option<String>,
    /// Approving or denying a request to join.
    pub(crate) knock: bool,
}

impl Intent {
    pub(crate) fn request(&self) -> AdminRequest {
        AdminRequest {
            action: self.action,
            user_id: self.user_id.clone(),
            expected: self.expected,
            reason: self.reason.clone(),
        }
    }
    /// The reviewed row still describes the member, or the threshold, in a
    /// fresher snapshot.
    pub(crate) fn still_matches(&self, snapshot: &RoomAdminSnapshot) -> bool {
        if let AdminAction::SetPermission(key, _) = self.action {
            let current = snapshot.permission(key).map(|row| row.level);
            return current.map(MemberPower::Level) == self.expected.power;
        }
        if let AdminAction::SetAccess(change) = self.action {
            let access = snapshot.access;
            return access.is_some_and(|access| access_current(change, access));
        }
        let row = snapshot.member(&self.user_id);
        let power = row.map(|member| member.power);
        let reviewed = self.expected.power;
        row.map(|member| member.membership) == self.expected.membership
            && reviewed.is_none_or(|expected| power == Some(expected))
    }
}

pub(crate) fn parse_level(text: &str) -> Result<i64, String> {
    let level = text
        .trim()
        .parse::<i64>()
        .map_err(|_| "Enter a whole-number power level, such as 0, 50 or 100.".to_owned())?;
    if !(-MAX_LEVEL..=MAX_LEVEL).contains(&level) {
        return Err("That power level is outside the range Matrix allows.".to_owned());
    }
    Ok(level)
}

fn parse_reason(input: &str) -> Result<Option<String>, String> {
    let reason = input.trim();
    if reason.len() > MAX_ADMIN_REASON_BYTES || reason.chars().any(char::is_control) {
        return Err(LONG_REASON.to_owned());
    }
    Ok((!reason.is_empty()).then(|| reason.to_owned()))
}

/// Build a change for the selected member from the panel's request kind.
/// Only actions the latest authoritative snapshot allows are admitted.
pub(crate) fn member_intent(
    kind: i32,
    member: &AdminMember,
    level_input: &str,
    reason_input: &str,
    snapshot: &RoomAdminSnapshot,
) -> Result<Intent, String> {
    let actions = member.actions;
    let knocking = member.membership == AdminMembership::Knock;
    let name = display_name(member);
    let action = match kind {
        KICK if actions.kick && !knocking => AdminAction::Kick,
        DENY if actions.kick && knocking => AdminAction::Kick,
        BAN if actions.ban => AdminAction::Ban,
        UNBAN if actions.unban => AdminAction::Unban,
        APPROVE if actions.approve => AdminAction::Invite,
        SET_LEVEL if actions.change_power => {
            let level = parse_level(level_input)?;
            if MemberPower::Level(level) == member.power {
                return Err(format!("{name} already has level {level}."));
            }
            if MemberPower::Level(level) > snapshot.own_power {
                return Err(format!(
                    "You can assign levels up to your own, {}.",
                    power_text(snapshot.own_power)
                ));
            }
            AdminAction::SetPowerLevel(level)
        }
        _ => {
            let missing = restriction(member, snapshot);
            if missing.is_empty() {
                return Err(format!("That action does not apply to {name} right now."));
            }
            return Err(missing);
        }
    };
    let reason = match action {
        AdminAction::SetPowerLevel(_) | AdminAction::Invite => None,
        _ => parse_reason(reason_input)?,
    };
    Ok(Intent {
        action,
        user_id: member.user_id.clone(),
        name,
        expected: MemberExpectation {
            membership: Some(member.membership),
            power: Some(member.power),
        },
        reason,
        knock: kind == APPROVE || kind == DENY,
    })
}

/// Invite by Matrix user ID. The roster may be bounded, so the session still
/// checks the server's current membership before sending anything.
pub(crate) fn invite_intent(input: &str, snapshot: &RoomAdminSnapshot) -> Result<Intent, String> {
    let user_id = input.trim();
    let shaped = user_id.len() <= MAX_USER_ID_BYTES
        && user_id.starts_with('@')
        && user_id.contains(':')
        && !user_id.chars().any(char::is_whitespace)
        && !user_id.chars().any(char::is_control);
    if !shaped {
        return Err(BAD_USER_ID.to_owned());
    }
    if !snapshot.own.invite {
        return Err(permission_text(AdminAction::Invite, snapshot));
    }
    let existing = snapshot.member(user_id);
    let name = existing.map_or_else(|| crate::directory_plain_text(user_id, 260), display_name);
    match existing.map(|member| member.membership) {
        Some(AdminMembership::Join) => Err(format!("{name} is already in this room.")),
        Some(AdminMembership::Invite) => Err(format!("{name} already has a pending invitation.")),
        Some(AdminMembership::Ban) => Err(format!("{name} is banned. Unban them before inviting.")),
        listed @ (Some(AdminMembership::Knock) | None) => Ok(Intent {
            action: AdminAction::Invite,
            user_id: user_id.to_owned(),
            name,
            expected: MemberExpectation {
                membership: listed,
                power: None,
            },
            reason: None,
            knock: listed.is_some(),
        }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Confirmation {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) action: &'static str,
    pub(crate) destructive: bool,
}

/// Kick, ban, unban, denying a request, every level change and every
/// permission change need a deliberate confirmation. Invitations and
/// approving a request do not.
pub(crate) fn confirmation(intent: &Intent, snapshot: &RoomAdminSnapshot) -> Option<Confirmation> {
    let own = snapshot.own_power;
    let own_row = snapshot.members.iter().find(|member| member.is_self);
    let is_self = own_row.is_some_and(|member| member.user_id == intent.user_id);
    let name = &intent.name;
    let mut confirmation = match intent.action {
        AdminAction::Invite => return None,
        AdminAction::Kick if intent.knock => Confirmation {
            title: format!("Deny {name}'s request?"),
            body: DENY_BODY.to_owned(),
            action: "Deny request",
            destructive: true,
        },
        AdminAction::Kick => Confirmation {
            title: format!("Kick {name}?"),
            body: KICK_BODY.to_owned(),
            action: "Kick",
            destructive: true,
        },
        AdminAction::Ban => Confirmation {
            title: format!("Ban {name}?"),
            body: BAN_BODY.to_owned(),
            action: "Ban",
            destructive: true,
        },
        AdminAction::Unban => Confirmation {
            title: format!("Unban {name}?"),
            body: UNBAN_BODY.to_owned(),
            action: "Unban",
            destructive: false,
        },
        AdminAction::SetPowerLevel(level) => {
            let current = intent.expected.power.unwrap_or(MemberPower::Level(0));
            let demotion = MemberPower::Level(level) < current;
            let irreversible = !is_self && MemberPower::Level(level) >= own;
            let from = power_text(current);
            let body = if is_self && demotion {
                format!("Your level drops to {level}. {SELF_DEMOTION}")
            } else if demotion {
                format!("{name}'s level drops from {from} to {level}. {DEMOTION}")
            } else if irreversible {
                format!("{name} gets level {level}, as high as yours. {IRREVERSIBLE}")
            } else {
                format!("{name}'s level changes from {from} to {level}.")
            };
            Confirmation {
                title: if demotion {
                    format!("Demote {name}?")
                } else {
                    format!("Change {name}'s level?")
                },
                body,
                action: if demotion { "Demote" } else { "Change level" },
                destructive: demotion || irreversible,
            }
        }
        AdminAction::SetPermission(key, level) => {
            let current = intent.expected.power.unwrap_or(MemberPower::Level(0));
            let widens = MemberPower::Level(level) < current;
            let from = power_text(current);
            // Raising the default gives everyone more power; lowering it can
            // also lower the operator's own inherited level.
            let follows_default = own_row.is_some_and(|member| !member.explicit_level);
            let mut body = if key == PermissionKey::UsersDefault {
                format!("Members without a level of their own move from {from} to level {level}.")
            } else if widens {
                format!("{name} will need level {level} instead of {from}. More members get it.")
            } else {
                format!("{name} will need level {level} instead of {from}. Fewer members keep it.")
            };
            let own_default = key == PermissionKey::UsersDefault && follows_default;
            if own_default {
                body.push(' ');
                body.push_str(OWN_DEFAULT);
            }
            let sensitive = matches!(
                key,
                PermissionKey::Kick
                    | PermissionKey::Ban
                    | PermissionKey::Redact
                    | PermissionKey::JoinRules
                    | PermissionKey::PowerLevels
            );
            let raises_default = key == PermissionKey::UsersDefault && !widens;
            Confirmation {
                title: format!("Change \u{201c}{name}\u{201d}?"),
                body,
                action: "Change permission",
                destructive: (sensitive && widens) || raises_default || (own_default && widens),
            }
        }
        AdminAction::SetAccess(AccessChange::History { from, to }) => Confirmation {
            title: "Change who can read history?".to_owned(),
            body: format!("New messages: {}. {HISTORY_BODY}", history_text(to)),
            action: "Change history",
            destructive: history_rank(to) > history_rank(from),
        },
        AdminAction::SetAccess(AccessChange::Guests { allow: true }) => Confirmation {
            title: "Allow guests to join?".to_owned(),
            body: GUESTS_ALLOW.to_owned(),
            action: "Allow guests",
            destructive: true,
        },
        AdminAction::SetAccess(AccessChange::Guests { allow: false }) => Confirmation {
            title: "Stop guests from joining?".to_owned(),
            body: GUESTS_FORBID.to_owned(),
            action: "Stop guests",
            destructive: false,
        },
    };
    if let Some(reason) = &intent.reason {
        confirmation.body.push_str("\n\nReason, visible to the room: ");
        confirmation.body.push_str(reason);
    }
    Some(confirmation)
}

pub(crate) fn pending_text(intent: &Intent) -> String {
    let verb = match intent.action {
        AdminAction::Invite if intent.knock => "Approving",
        AdminAction::Invite => "Inviting",
        AdminAction::Kick if intent.knock => "Denying",
        AdminAction::Kick => "Kicking",
        AdminAction::Ban => "Banning",
        AdminAction::Unban => "Unbanning",
        AdminAction::SetPowerLevel(_) => "Changing the level of",
        AdminAction::SetPermission(..) | AdminAction::SetAccess(_) => {
            let name = &intent.name;
            return format!("Changing \u{201c}{name}\u{201d}… Waiting for the homeserver.");
        }
    };
    format!("{verb} {}… Waiting for the homeserver.", intent.name)
}

fn observed_text(seen: ObservedTarget) -> String {
    let membership = match seen.membership {
        ObservedMembership::Join => "in the room",
        ObservedMembership::Invite => "invited",
        ObservedMembership::Knock => "asking to join",
        ObservedMembership::Ban => "banned",
        ObservedMembership::Leave | ObservedMembership::None => "not in the room",
    };
    format!("{membership} with {}", power_text(seen.power))
}

/// Report only what the homeserver accepted and what reading it back showed.
pub(crate) fn outcome_text(intent: &Intent, outcome: &AdminOutcome) -> String {
    let name = &intent.name;
    let Some(seen) = outcome.observed else {
        return format!("The homeserver accepted the change for {name}. {UNREAD}");
    };
    if let AdminAction::SetAccess(change) = intent.action {
        return access_outcome_text(change, outcome.confirmed);
    }
    if let (AdminAction::SetPermission(..), false) = (intent.action, outcome.confirmed) {
        let seen = power_text(seen.power);
        return format!("The homeserver accepted the change, but {name} now needs {seen}.");
    }
    if !outcome.confirmed {
        let seen = observed_text(seen);
        return format!("The homeserver accepted the change but reports {name} as {seen}.");
    }
    match intent.action {
        AdminAction::Invite if intent.knock => format!("Approved: {name} is invited and can join."),
        AdminAction::Invite => format!("Invited {name}. The homeserver confirmed the invitation."),
        AdminAction::Kick if intent.knock => format!("Denied {name}'s request to join."),
        AdminAction::Kick => format!("Kicked {name}. The homeserver confirmed they left."),
        AdminAction::Ban => format!("Banned {name}. The homeserver confirmed the ban."),
        AdminAction::Unban => format!("Unbanned {name}. The homeserver confirmed it."),
        AdminAction::SetPowerLevel(level) => {
            format!("{name} now has level {level}. The homeserver confirmed it.")
        }
        AdminAction::SetPermission(PermissionKey::UsersDefault, level) => {
            format!("Members now start at level {level}. The homeserver confirmed it.")
        }
        AdminAction::SetPermission(_, level) => {
            format!("{name} now needs level {level}. The homeserver confirmed it.")
        }
        AdminAction::SetAccess(change) => access_outcome_text(change, outcome.confirmed),
    }
}

fn access_outcome_text(change: AccessChange, confirmed: bool) -> String {
    if !confirmed {
        return "The homeserver accepted the change, but the room shows another setting.".to_owned();
    }
    match change {
        AccessChange::History { to, .. } => {
            let text = history_text(to);
            format!("New messages: {text}. The homeserver confirmed it.")
        }
        AccessChange::Guests { allow: true } => {
            "Guests can now join. The homeserver confirmed it.".to_owned()
        }
        AccessChange::Guests { allow: false } => {
            "Guests can no longer join. The homeserver confirmed it.".to_owned()
        }
    }
}

pub(crate) fn permission_text(action: AdminAction, snapshot: &RoomAdminSnapshot) -> String {
    if let AdminAction::SetPermission(key, _) = action {
        return permission_restriction(key, snapshot);
    }
    if let AdminAction::SetAccess(change) = action {
        let Some(access) = snapshot.access else {
            return UNKNOWN_RULES.to_owned();
        };
        let level = match change {
            AccessChange::History { .. } => access.history_level,
            AccessChange::Guests { .. } => access.guests_level,
        };
        return access_restriction(level, snapshot);
    }
    let own = power_text(snapshot.own_power);
    let Some(levels) = snapshot.thresholds else {
        return UNKNOWN_RULES.to_owned();
    };
    let (verb, needed) = match action {
        AdminAction::Invite => ("Inviting", levels.invite),
        AdminAction::Kick => ("Kicking", levels.kick),
        AdminAction::Ban => ("Banning", levels.ban),
        AdminAction::Unban => ("Unbanning", levels.ban.max(levels.kick)),
        AdminAction::SetPowerLevel(_)
        | AdminAction::SetPermission(..)
        | AdminAction::SetAccess(_) => ("Changing levels", levels.change_power_levels),
    };
    if action == AdminAction::Invite || snapshot.own_power < MemberPower::Level(needed) {
        format!("{verb} needs level {needed}; you have {own}. {ASK_HIGHER}")
    } else {
        format!("{verb} this member needs a level above theirs; you have {own}.")
    }
}

/// Why the selected member's actions are unavailable, or empty when the
/// snapshot allows every action that applies to their state.
pub(crate) fn restriction(member: &AdminMember, snapshot: &RoomAdminSnapshot) -> String {
    if member.power == MemberPower::Creator {
        return CREATOR.to_owned();
    }
    let actions = member.actions;
    if member.is_self && actions.change_power {
        return "This is you. You can lower your own level; use Leave room to leave.".to_owned();
    }
    if member.is_self {
        return "This is you. Use Leave room to leave.".to_owned();
    }
    let missing = match member.membership {
        AdminMembership::Ban if !actions.unban => Some(AdminAction::Unban),
        AdminMembership::Knock if !actions.approve => Some(AdminAction::Invite),
        AdminMembership::Knock if !actions.kick => Some(AdminAction::Kick),
        AdminMembership::Join | AdminMembership::Invite if !actions.kick => Some(AdminAction::Kick),
        AdminMembership::Join | AdminMembership::Invite if !actions.ban => Some(AdminAction::Ban),
        AdminMembership::Join | AdminMembership::Invite if !actions.change_power => {
            Some(AdminAction::SetPowerLevel(0))
        }
        _ => None,
    };
    missing.map_or_else(String::new, |action| permission_text(action, snapshot))
}

pub(crate) fn load_error_text(error: RoomAdminError) -> String {
    let text = match error {
        RoomAdminError::Cancelled | RoomAdminError::AccountChanged => "",
        RoomAdminError::NotJoined => "Join this room to see and manage its members.",
        RoomAdminError::RateLimited => "The homeserver is limiting requests. Wait, then refresh.",
        RoomAdminError::InvalidState => "This room returned roles Morpheus cannot read safely.",
        _ => "Room members could not be loaded. Check the connection, then refresh.",
    };
    text.to_owned()
}

/// An actionable message for a failed change. Failures after sending may
/// still have applied, so the roster is re-read rather than the change resent.
pub(crate) fn error_text(
    error: RoomAdminError,
    intent: &Intent,
    snapshot: Option<&RoomAdminSnapshot>,
) -> String {
    let name = &intent.name;
    let setting = matches!(
        intent.action,
        AdminAction::SetPermission(..) | AdminAction::SetAccess(_)
    );
    if setting {
        match (error, snapshot) {
            (RoomAdminError::TargetChanged, _) => return SETTING_CHANGED.to_owned(),
            (RoomAdminError::NotApplicable, _) => {
                return "The room already uses that setting.".to_owned();
            }
            (RoomAdminError::InsufficientPower, Some(snapshot)) => {
                return permission_text(intent.action, snapshot);
            }
            (RoomAdminError::Forbidden, _) => {
                return format!("The homeserver refused this change. {REFUSED_PERMISSION}");
            }
            _ => {}
        }
    }
    let fixed = match error {
        RoomAdminError::Cancelled | RoomAdminError::AccountChanged => CLOSED,
        RoomAdminError::NotJoined => "Join this room to manage its members.",
        RoomAdminError::Unavailable | RoomAdminError::TimedOut => UNCONFIRMED,
        RoomAdminError::RateLimited => "The homeserver is limiting requests. Wait, then try again.",
        RoomAdminError::InvalidState => UNREADABLE,
        RoomAdminError::CreatorProtected => CREATOR,
        RoomAdminError::SelfTarget => "Use Leave room to remove yourself.",
        RoomAdminError::InvalidUserId => BAD_USER_ID,
        RoomAdminError::InvalidPowerLevel => "That power level is outside the range Matrix allows.",
        RoomAdminError::InvalidReason => LONG_REASON,
        RoomAdminError::InvalidRoom => "Enter a room ID, room address or Matrix link.",
        RoomAdminError::RoomNotFound => "No room uses that address.",
        RoomAdminError::NoRoute => NO_ROUTE,
        RoomAdminError::UnsupportedContent => UNSUPPORTED_CONTENT,
        RoomAdminError::TooManyChanges => "This change would rewrite too many entries at once.",
        RoomAdminError::InsufficientPower => {
            return snapshot.map_or_else(
                || "Your current power level does not allow this change.".to_owned(),
                |snapshot| permission_text(intent.action, snapshot),
            );
        }
        RoomAdminError::Forbidden => {
            return format!("The homeserver refused this change for {name}. {REFUSED}");
        }
        RoomAdminError::TargetChanged => {
            return format!("{name} changed on the server since you reviewed them. {REVIEW}");
        }
        RoomAdminError::AlreadyJoined => return format!("{name} is already in this room."),
        RoomAdminError::AlreadyInvited => {
            return format!("{name} already has a pending invitation.");
        }
        RoomAdminError::TargetBanned => {
            return format!("{name} is banned. Unban them before inviting.");
        }
        RoomAdminError::NotApplicable => {
            return format!("This action does not apply to {name}'s current state.");
        }
    };
    fixed.to_owned()
}

/// Input mistakes are rejected before anything is sent; other failures
/// re-read the roster so the operator sees the authoritative state.
pub(crate) fn reread_after(error: RoomAdminError) -> bool {
    !matches!(
        error,
        RoomAdminError::Cancelled
            | RoomAdminError::AccountChanged
            | RoomAdminError::InvalidUserId
            | RoomAdminError::InvalidPowerLevel
            | RoomAdminError::InvalidReason
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::{MemberActions, OwnAdminPermissions};

    fn member(user: &str, name: Option<&str>, membership: AdminMembership) -> AdminMember {
        AdminMember {
            user_id: user.to_owned(),
            display_name: name.map(str::to_owned),
            membership,
            power: MemberPower::Level(0),
            explicit_level: false,
            is_self: false,
            actions: MemberActions::default(),
        }
    }
    fn joined(user: String) -> AdminMember {
        member(&user, None, AdminMembership::Join)
    }
    fn roster(members: Vec<AdminMember>, own: i64) -> RoomAdminSnapshot {
        RoomAdminSnapshot {
            members,
            truncated: false,
            counts: MembershipCounts::default(),
            own_power: MemberPower::Level(own),
            own: OwnAdminPermissions {
                invite: true,
                kick: true,
                ban: true,
                unban: true,
                change_power_levels: true,
            },
            thresholds: Some(PowerThresholds {
                users_default: 0,
                invite: 0,
                kick: 50,
                ban: 50,
                redact: 50,
                change_power_levels: 50,
            }),
            permissions: Vec::new(),
            access: None,
            room_version: "11".to_owned(),
        }
    }
    fn intent(action: AdminAction, level: i64, knock: bool) -> Intent {
        Intent {
            action,
            user_id: "@jules:example.org".to_owned(),
            name: "Jules".to_owned(),
            expected: MemberExpectation {
                membership: Some(AdminMembership::Join),
                power: Some(MemberPower::Level(level)),
            },
            reason: None,
            knock,
        }
    }

    #[test]
    fn roster_search_is_bounded_case_insensitive_and_counts_every_match() {
        let mut members = (0..250)
            .map(|index| joined(format!("@user{index}:example.org")))
            .collect::<Vec<_>>();
        let casey = Some("Casey Ödegaard");
        members.push(member("@casey:example.org", casey, AdminMembership::Ban));
        let snapshot = roster(members, 50);
        let (shown, total) = visible(&snapshot, "", Filter::All);
        assert_eq!((shown.len(), total), (MAX_VISIBLE_ROWS, 251));
        assert!(shown_text(shown.len(), total, false).contains("200 of 251"));
        let (shown, total) = visible(&snapshot, "  ÖDEGAARD ", Filter::All);
        assert_eq!((shown.len(), total), (1, 1));
        assert_eq!(shown[0].user_id, "@casey:example.org");
        assert_eq!(visible(&snapshot, "casey", Filter::Joined).1, 0);
        assert_eq!(visible(&snapshot, "", Filter::Banned).1, 1);
        assert_eq!(visible(&snapshot, &"x".repeat(10_000), Filter::All).1, 0);
    }

    #[test]
    fn member_rows_sanitize_untrusted_identity() {
        let spoofed = Some("Alice\u{202e}Admin\u{200b}");
        let alice = member("@alice:example.org", spoofed, AdminMembership::Join);
        assert_eq!(display_name(&alice), "Alice Admin");
        let bob = member("@bob:example.org", None, AdminMembership::Invite);
        assert_eq!(display_name(&bob), "bob");
        let thresholds = roster(Vec::new(), 0).thresholds;
        assert_eq!(
            row_detail(&bob, thresholds.as_ref()),
            "Member · level 0 · Invited"
        );
    }

    #[test]
    fn destructive_moderation_and_role_changes_require_deliberate_confirmation() {
        let jules = member("@jules:example.org", None, AdminMembership::Join);
        let snapshot = roster(vec![jules.clone()], 100);
        let review = |intent: &Intent| confirmation(intent, &snapshot);
        assert!(review(&intent(AdminAction::Invite, 0, false)).is_none());
        assert!(review(&intent(AdminAction::Invite, 0, true)).is_none());
        for action in [AdminAction::Kick, AdminAction::Ban] {
            assert!(review(&intent(action, 0, false)).unwrap().destructive);
        }
        let deny = review(&intent(AdminAction::Kick, 0, true)).unwrap();
        assert!(deny.destructive && deny.title.starts_with("Deny"));
        let unban = review(&intent(AdminAction::Unban, 0, false)).unwrap();
        assert!(!unban.destructive);
        let demotion = review(&intent(AdminAction::SetPowerLevel(0), 50, false)).unwrap();
        assert!(demotion.destructive && demotion.action == "Demote");
        let routine = review(&intent(AdminAction::SetPowerLevel(50), 0, false)).unwrap();
        assert!(!routine.destructive);
        let peer = review(&intent(AdminAction::SetPowerLevel(100), 0, false)).unwrap();
        assert!(peer.destructive, "cannot be lowered again");
        let mut me = jules;
        me.is_self = true;
        let own_snapshot = roster(vec![me], 100);
        let own_demotion = intent(AdminAction::SetPowerLevel(0), 100, false);
        let own_demotion = confirmation(&own_demotion, &own_snapshot).unwrap();
        assert!(own_demotion.destructive && own_demotion.body.contains("yourself"));
        let mut reasoned = intent(AdminAction::Ban, 0, false);
        reasoned.reason = Some("spam".to_owned());
        let shown = review(&reasoned).unwrap();
        assert!(shown.body.contains("visible to the room: spam"));
    }

    #[test]
    fn member_intents_follow_snapshot_permissions_and_capture_the_reviewed_row() {
        let mut jules = member("@jules:example.org", None, AdminMembership::Join);
        let snapshot = roster(vec![jules.clone()], 50);
        assert!(member_intent(KICK, &jules, "", "", &snapshot).is_err());
        jules.actions = MemberActions {
            kick: true,
            change_power: true,
            ..MemberActions::default()
        };
        let kick = member_intent(KICK, &jules, "", " spam ", &snapshot).unwrap();
        assert_eq!(kick.action, AdminAction::Kick);
        assert_eq!(kick.reason.as_deref(), Some("spam"));
        assert_eq!(kick.expected.membership, Some(AdminMembership::Join));
        assert_eq!(kick.expected.power, Some(MemberPower::Level(0)));
        assert!(member_intent(DENY, &jules, "", "", &snapshot).is_err());
        assert!(member_intent(BAN, &jules, "", "", &snapshot).is_err());
        let long = "x".repeat(MAX_ADMIN_REASON_BYTES + 1);
        assert!(member_intent(KICK, &jules, "", &long, &snapshot).is_err());
        let level = member_intent(SET_LEVEL, &jules, " 50 ", "", &snapshot).unwrap();
        assert_eq!(level.action, AdminAction::SetPowerLevel(50));
        for invalid in ["51", "0", "fifty", "9007199254740992"] {
            assert!(member_intent(SET_LEVEL, &jules, invalid, "", &snapshot).is_err());
        }
        let mut knocking = member("@kim:example.org", None, AdminMembership::Knock);
        knocking.actions = MemberActions {
            kick: true,
            approve: true,
            ..MemberActions::default()
        };
        let approve = member_intent(APPROVE, &knocking, "", "", &snapshot).unwrap();
        assert!(approve.action == AdminAction::Invite && approve.knock);
        assert!(member_intent(KICK, &knocking, "", "", &snapshot).is_err());
        let deny = member_intent(DENY, &knocking, "", "", &snapshot).unwrap();
        assert!(deny.action == AdminAction::Kick && deny.knock);
    }

    #[test]
    fn invites_refuse_listed_members_and_need_invite_permission() {
        let members = vec![
            member("@in:example.org", None, AdminMembership::Join),
            member("@banned:example.org", None, AdminMembership::Ban),
            member("@kim:example.org", None, AdminMembership::Knock),
        ];
        let mut snapshot = roster(members, 0);
        for refused in ["@in:example.org", "@banned:example.org", "name", "@a b:c"] {
            assert!(invite_intent(refused, &snapshot).is_err());
        }
        let new = invite_intent(" @new:example.org ", &snapshot).unwrap();
        assert_eq!(new.user_id, "@new:example.org");
        assert_eq!(new.expected.membership, None);
        assert!(!new.knock);
        let approval = invite_intent("@kim:example.org", &snapshot).unwrap();
        assert!(approval.knock);
        assert_eq!(approval.expected.membership, Some(AdminMembership::Knock));
        snapshot.own.invite = false;
        assert!(invite_intent("@new:example.org", &snapshot).is_err());
    }

    #[test]
    fn a_reviewed_choice_is_stale_once_the_member_changes() {
        let reviewed = intent(AdminAction::Ban, 0, false);
        let jules = member("@jules:example.org", None, AdminMembership::Join);
        assert!(reviewed.still_matches(&roster(vec![jules.clone()], 50)));
        let mut promoted = jules.clone();
        promoted.power = MemberPower::Level(50);
        assert!(!reviewed.still_matches(&roster(vec![promoted], 50)));
        let mut banned = jules;
        banned.membership = AdminMembership::Ban;
        assert!(!reviewed.still_matches(&roster(vec![banned], 50)));
        assert!(!reviewed.still_matches(&roster(Vec::new(), 50)));
    }

    fn with_permissions(mut snapshot: RoomAdminSnapshot, editable: bool) -> RoomAdminSnapshot {
        snapshot.permissions = PERMISSION_KEYS
            .iter()
            .map(|&key| PermissionSetting {
                key,
                level: match key {
                    PermissionKey::UsersDefault => 0,
                    _ => 50,
                },
                editable,
            })
            .collect();
        snapshot
    }
    fn index(key: PermissionKey) -> i32 {
        let position = PERMISSION_KEYS.iter().position(|item| *item == key);
        i32::try_from(position.unwrap()).unwrap()
    }

    #[test]
    fn permission_changes_stay_within_the_operators_level_and_capture_the_reviewed_value() {
        let snapshot = with_permissions(roster(Vec::new(), 50), true);
        let kick = index(PermissionKey::Kick);
        let lowered = permission_intent(kick, " 0 ", &snapshot).unwrap();
        let expected = AdminAction::SetPermission(PermissionKey::Kick, 0);
        assert_eq!(lowered.action, expected);
        assert_eq!(lowered.expected.power, Some(MemberPower::Level(50)));
        assert!(lowered.still_matches(&snapshot));
        for refused in ["60", "50", "many"] {
            assert!(permission_intent(kick, refused, &snapshot).is_err());
        }
        assert!(permission_intent(-1, "0", &snapshot).is_err());
        assert!(permission_intent(12, "0", &snapshot).is_err());
        let locked = with_permissions(roster(Vec::new(), 50), false);
        assert!(permission_intent(kick, "0", &locked).is_err());
        let mut moved = snapshot.clone();
        moved.permissions[3].level = 40;
        assert!(!lowered.still_matches(&moved));
    }

    #[test]
    fn widening_moderation_and_raising_the_default_are_destructive() {
        let mut me = member("@me:example.org", None, AdminMembership::Join);
        me.is_self = true;
        let snapshot = with_permissions(roster(vec![me], 100), true);
        let change = |key, level| permission_intent(index(key), level, &snapshot).unwrap();
        let review = |intent: &Intent| confirmation(intent, &snapshot).unwrap();
        assert!(review(&change(PermissionKey::Ban, "0")).destructive);
        assert!(!review(&change(PermissionKey::Ban, "100")).destructive);
        assert!(!review(&change(PermissionKey::EventsDefault, "0")).destructive);
        assert!(review(&change(PermissionKey::UsersDefault, "10")).destructive);
        let lowered = review(&change(PermissionKey::UsersDefault, "-5"));
        assert!(lowered.destructive && lowered.body.contains(OWN_DEFAULT));
    }

    fn with_access(editable: bool) -> RoomAdminSnapshot {
        let mut snapshot = roster(Vec::new(), 50);
        snapshot.access = Some(AccessSettings {
            history: Some(HistoryChoice::Joined),
            history_level: 50,
            history_editable: editable,
            guests_can_join: Some(false),
            guests_level: 50,
            guests_editable: editable,
        });
        snapshot
    }

    #[test]
    fn access_changes_are_reviewed_confirmed_and_withdrawn_when_stale() {
        let snapshot = with_access(true);
        let open = access_intent(ACCESS_HISTORY, 3, &snapshot).unwrap();
        let widen = AccessChange::History {
            from: HistoryChoice::Joined,
            to: HistoryChoice::WorldReadable,
        };
        assert_eq!(open.action, AdminAction::SetAccess(widen));
        assert!(confirmation(&open, &snapshot).unwrap().destructive);
        assert!(open.still_matches(&snapshot));
        let narrow = access_intent(ACCESS_HISTORY, 0, &snapshot);
        assert!(narrow.is_err(), "already the room's setting");
        let guests = access_intent(ACCESS_GUESTS, 1, &snapshot).unwrap();
        assert!(confirmation(&guests, &snapshot).unwrap().destructive);
        assert!(access_intent(ACCESS_GUESTS, 0, &snapshot).is_err());
        assert!(access_intent(ACCESS_HISTORY, 3, &with_access(false)).is_err());

        let mut moved = snapshot.clone();
        if let Some(access) = moved.access.as_mut() {
            access.history = Some(HistoryChoice::Shared);
        }
        assert!(!open.still_matches(&moved));
        let mut custom = snapshot.clone();
        if let Some(access) = custom.access.as_mut() {
            access.history = None;
        }
        assert!(access_intent(ACCESS_HISTORY, 3, &custom).is_err());

        let rows = access_rows(&snapshot);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].choice, rows[1].choice), (0, 0));
        let unlocked = |row: &AccessRow| row.editable && row.restriction.is_empty();
        assert!(rows.iter().all(unlocked));
    }

    #[test]
    fn failures_after_sending_reread_instead_of_resending() {
        for error in [
            RoomAdminError::Unavailable,
            RoomAdminError::TimedOut,
            RoomAdminError::Forbidden,
            RoomAdminError::TargetChanged,
        ] {
            assert!(reread_after(error));
        }
        assert!(!reread_after(RoomAdminError::InvalidReason));
        assert!(!reread_after(RoomAdminError::AccountChanged));
        let kick = intent(AdminAction::Kick, 0, false);
        let text = error_text(RoomAdminError::Unavailable, &kick, None);
        assert!(text.contains("may still apply"));
    }
}
