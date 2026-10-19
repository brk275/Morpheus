//! Pure rules for native room administration: bounded roster search, role
//! labels, deliberate confirmation and actionable messages. The UI adapter in
//! `ui.rs` holds the account selection, the exact room and a per-panel
//! generation across every asynchronous result.

pub(crate) mod ui;

pub(crate) use ui::clear_presentation;

use morpheus_matrix::{
    AccessChange, AccessSettings, AddressSettings, AdminAction, AdminMember, AdminMembership,
    AdminOutcome, AdminRequest, HistoryChoice, MAX_ADMIN_REASON_BYTES, MAX_ALIAS_BYTES,
    MAX_BLOCKED_SERVERS, MAX_REMOVED_MESSAGES, MAX_SERVER_BYTES, MemberExpectation, MemberPower,
    MembershipCounts, MessageRemoval, ObservedMembership, ObservedTarget, PERMISSION_KEYS,
    PermissionKey, PermissionSetting, PowerThresholds, RoomAdminError, RoomAdminSnapshot,
    ServerAclSettings, UpgradeSettings,
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
const REVOKE_BODY: &str =
    "They can no longer join with this invitation. You can invite them again later.";
const BAN_BODY: &str = "They leave the room and cannot rejoin until someone unbans them.";
const BAN_ABSENT_BODY: &str =
    "They are not in the room now. They cannot join or be invited until someone unbans them.";
const UNBAN_BODY: &str =
    "They can join again if the room's access rules allow it. This does not invite them.";
const REMOVE_BODY: &str = "including files and reactions, are removed for everyone, newest \
     first. This cannot be undone. Older messages and their changes to the room stay.";
const REMOVE_UNFINISHED: &str =
    "Removing messages did not finish. Some may already be gone; check the conversation.";
const LEAVE_INSTEAD: &str = "Use Leave room to remove yourself.";
const OWN_MESSAGES: &str = "Remove your own messages from the conversation itself.";
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
const REFUSED_PERMISSION: &str = "Your permissions may have changed; review them again.";
const OWN_DEFAULT: &str = "Your own level follows the default, so it changes too.";
const CUSTOM_ACCESS: &str = "This room uses a setting Morpheus does not edit.";
const CUSTOM_HISTORY: &str = "Custom history setting";
const HISTORY_BODY: &str = "Messages already sent keep the visibility they were sent with.";
const GUESTS_ALLOW: &str = "Guest accounts can join whenever the room's join rules let them in.";
const GUESTS_FORBID: &str = "Guest accounts can no longer join. Guests already here stay.";
const ENCRYPT_BODY: &str = "From now on only members' devices can read new messages. It cannot be \
     turned off again, and bots or bridges without encryption support stop reading new messages. \
     Messages already sent stay as they are.";
const ENCRYPTION_ON: &str = "Encryption is on. Matrix rooms cannot turn it off again.";
const UPGRADE_BODY: &str = "Members must join the new room themselves; this room keeps its \
     history but no longer takes messages. Spaces that list this room keep pointing at it.";
const UPGRADE_REPLACED: &str = "This room was already replaced by a newer one.";
const UPGRADE_CURRENT: &str = "This room already uses the version your homeserver recommends.";
const SETTING_CHANGED: &str = "This setting changed on the server. Review it again.";
const BAD_SERVER: &str =
    "Enter a server name such as spam.example, or a pattern like *.spam.example.";
const SERVERS_UNCONFIRMED: &str =
    "The homeserver accepted the change, but the block list reads differently.";
const OWN_SERVER: &str =
    "That would block your own homeserver, and this room would stop accepting your messages.";
const CUSTOM_SERVERS: &str =
    "This room limits servers with an allow list or settings Morpheus does not edit.";
const BLOCK_BODY: &str = "can no longer join, send messages or receive new ones in this room. \
     Members already here stay listed but are cut off. Other rooms are unaffected.";
const CUSTOM_ADDRESS: &str = "This room's addresses use settings Morpheus does not edit.";
const ADDRESS_TAKEN: &str = "That address already belongs to another room. Choose another name.";
const ADDRESS_UNCONFIRMED: &str =
    "The homeserver accepted the change, but the room shows another main address.";
const LISTING_UNKNOWN: &str = "Your homeserver did not say whether it lists this room.";
const LIST_BODY: &str = "People on your homeserver, and people searching from other servers, \
     can find this room by its name, topic and address. Who can join is unchanged.";
const UNLIST_BODY: &str =
    "People can no longer find it in the directory. Members and addresses are unaffected.";
/// Blocked servers named in the row summary; the rest are counted.
const MAX_LISTED_SERVERS: usize = 20;

pub(crate) const KICK: i32 = 1;
pub(crate) const BAN: i32 = 2;
pub(crate) const UNBAN: i32 = 3;
pub(crate) const SET_LEVEL: i32 = 4;
pub(crate) const APPROVE: i32 = 5;
pub(crate) const DENY: i32 = 6;
pub(crate) const REMOVE_MESSAGES: i32 = 7;
/// Ban the Matrix user ID typed in the invite field.
pub(crate) const BAN_ID: i32 = 8;
/// Remove the recent messages of the Matrix user ID typed in the invite field.
pub(crate) const REMOVE_ID: i32 = 9;

/// Access rows in the permissions view: history visibility, guest access and
/// end-to-end encryption.
pub(crate) const ACCESS_HISTORY: i32 = 1;
pub(crate) const ACCESS_GUESTS: i32 = 2;
pub(crate) const ACCESS_ENCRYPTION: i32 = 3;
pub(crate) const ACCESS_UPGRADE: i32 = 4;
pub(crate) const ACCESS_SERVERS: i32 = 5;
pub(crate) const ACCESS_DIRECTORY: i32 = 6;
pub(crate) const ACCESS_ADDRESS: i32 = 7;
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
    /// History: index in `HISTORY_CHOICES`. Guests: 1 allowed, 0 not.
    /// Encryption: 1 on, 0 off. Version: 0 upgradable, 1 current, 2
    /// replaced. -1 custom.
    pub(crate) choice: i32,
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) summary: String,
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
        ACCESS_ENCRYPTION if access.encrypted => return ENCRYPTION_ON.to_owned(),
        ACCESS_ENCRYPTION => (true, access.encryption_editable, access.encryption_level),
        ACCESS_DIRECTORY if access.listed.is_none() => return LISTING_UNKNOWN.to_owned(),
        ACCESS_DIRECTORY => (true, access.listing_editable, access.listing_level),
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
    let encryption = if access.encrypted { "On" } else { "Off" };
    let history = access.history.map_or(CUSTOM_HISTORY, history_text);
    let listing = match access.listed {
        Some(true) => "Listed in the room directory",
        Some(false) => "Not listed",
        None => "Your homeserver did not say",
    };
    let mut rows = vec![
        AccessRow {
            kind: ACCESS_HISTORY,
            choice: access.history.map_or(-1, history_rank),
            label: "History visibility",
            description: "Who can read new messages in this room.",
            summary: history.to_owned(),
            editable: access.history_editable,
            restriction: access_lock(ACCESS_HISTORY, access, snapshot),
        },
        AccessRow {
            kind: ACCESS_GUESTS,
            choice: access.guests_can_join.map_or(-1, i32::from),
            label: "Guest access",
            description: "Whether guest accounts can join when the join rules let them.",
            summary: guests.to_owned(),
            editable: access.guests_editable,
            restriction: access_lock(ACCESS_GUESTS, access, snapshot),
        },
        AccessRow {
            kind: ACCESS_ENCRYPTION,
            choice: i32::from(access.encrypted),
            label: "End-to-end encryption",
            description: "Whether only members' devices can read new messages. Once on, it \
                stays on.",
            summary: encryption.to_owned(),
            editable: access.encryption_editable,
            restriction: access_lock(ACCESS_ENCRYPTION, access, snapshot),
        },
    ];
    rows.push(AccessRow {
        kind: ACCESS_DIRECTORY,
        choice: access.listed.map_or(-1, i32::from),
        label: "Room directory",
        description: "Whether people can find this room in your homeserver's public room \
            directory. Who can join is set separately.",
        summary: listing.to_owned(),
        editable: access.listing_editable,
        restriction: access_lock(ACCESS_DIRECTORY, access, snapshot),
    });
    if let Some(addresses) = &snapshot.addresses {
        rows.push(address_row(addresses, snapshot));
    }
    if let Some(upgrade) = &snapshot.upgrade {
        rows.push(upgrade_row(upgrade, snapshot));
    }
    if let Some(servers) = &snapshot.servers {
        rows.push(servers_row(servers, snapshot));
    }
    rows
}

fn address_row(addresses: &AddressSettings, snapshot: &RoomAdminSnapshot) -> AccessRow {
    let main = addresses.main.as_deref();
    let main = main.map(|main| crate::directory_plain_text(main, 100));
    let mut summary = main.unwrap_or_else(|| "No main address".to_owned());
    match addresses.alternatives.len() {
        0 => {}
        1 => summary.push_str(" · 1 other address"),
        count => summary.push_str(&format!(" · {count} other addresses")),
    }
    let restriction = if addresses.editable {
        String::new()
    } else if !addresses.known {
        CUSTOM_ADDRESS.to_owned()
    } else {
        access_restriction(addresses.level, snapshot)
    };
    AccessRow {
        kind: ACCESS_ADDRESS,
        choice: i32::from(addresses.main.is_some()),
        label: "Main address",
        description: "The address people use to find and share this room, like #name:server.",
        summary,
        editable: addresses.editable,
        restriction,
    }
}

/// A room address on the account's homeserver from a name or a full
/// `#name:server`, as the session accepts it.
fn parse_address(input: &str, server: &str) -> Result<String, String> {
    let bad = || format!("Use a name, or #name:{server}. Addresses are made on your homeserver.");
    let input = input.trim().to_ascii_lowercase();
    let suffix = format!(":{}", server.to_ascii_lowercase());
    let name = match input.strip_prefix('#') {
        Some(full) => full.strip_suffix(&suffix).ok_or_else(bad)?,
        None => input.as_str(),
    };
    let allowed = |byte: u8| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'=' | b'-');
    let alias = format!("#{name}:{server}");
    if name.is_empty() || !name.bytes().all(allowed) || alias.len() > MAX_ALIAS_BYTES {
        return Err(bad());
    }
    Ok(alias)
}

/// Make an address the room's main address. The session creates it on the
/// account's homeserver when it is free and refuses one another room uses.
pub(crate) fn address_intent(input: &str, snapshot: &RoomAdminSnapshot) -> Result<Intent, String> {
    let addresses = snapshot.addresses.as_ref();
    let addresses = addresses.ok_or_else(|| UNKNOWN_RULES.to_owned())?;
    if !addresses.known {
        return Err(CUSTOM_ADDRESS.to_owned());
    }
    if !addresses.editable {
        return Err(access_restriction(addresses.level, snapshot));
    }
    let alias = parse_address(input, &addresses.server)?;
    if addresses.main.as_deref() == Some(alias.as_str()) {
        return Err("That is already the main address.".to_owned());
    }
    Ok(Intent {
        action: AdminAction::SetAccess(AccessChange::Address),
        user_id: alias,
        name: "Main address".to_owned(),
        expected: MemberExpectation {
            membership: None,
            power: None,
        },
        reason: None,
        knock: false,
    })
}

fn servers_row(servers: &ServerAclSettings, snapshot: &RoomAdminSnapshot) -> AccessRow {
    let count = servers.denied.len();
    let mut summary = match count {
        0 => "No servers blocked".to_owned(),
        1 => "1 server blocked: ".to_owned(),
        _ => format!("{count} servers blocked: "),
    };
    let names = servers.denied.iter().take(MAX_LISTED_SERVERS);
    let names = names.map(|server| crate::directory_plain_text(server, 64));
    summary.push_str(&names.collect::<Vec<_>>().join(", "));
    if count > MAX_LISTED_SERVERS {
        summary.push_str(&format!(" and {} more", count - MAX_LISTED_SERVERS));
    }
    let restriction = if servers.editable {
        String::new()
    } else if !servers.known {
        CUSTOM_SERVERS.to_owned()
    } else {
        access_restriction(servers.level, snapshot)
    };
    AccessRow {
        kind: ACCESS_SERVERS,
        choice: i32::try_from(count).unwrap_or(i32::MAX),
        label: "Blocked servers",
        description: "Homeservers whose people cannot join, send or receive new messages here.",
        summary,
        editable: servers.editable,
        restriction,
    }
}

/// The block list names this server or pattern; matching ignores case.
fn blocks(denied: &[String], server: &str) -> bool {
    denied.iter().any(|item| item.eq_ignore_ascii_case(server))
}

/// A server name or pattern as the session accepts it: host characters
/// with `*` and `?` wildcards and no port, lowercased.
fn parse_server(input: &str) -> Result<String, String> {
    let server = input.trim().to_ascii_lowercase();
    let host = |byte: u8| byte.is_ascii_alphanumeric() || b".-*?".contains(&byte);
    let valid = !server.is_empty() && server.len() <= MAX_SERVER_BYTES && server.bytes().all(host);
    if valid {
        Ok(server)
    } else {
        Err(BAD_SERVER.to_owned())
    }
}

/// Block or unblock one server. The session re-reads the list, keeps the
/// rest of it, and refuses a block that would also match the account's own
/// homeserver.
pub(crate) fn server_intent(
    input: &str,
    block: bool,
    snapshot: &RoomAdminSnapshot,
) -> Result<Intent, String> {
    let servers = snapshot.servers.as_ref();
    let servers = servers.ok_or_else(|| UNKNOWN_RULES.to_owned())?;
    if !servers.known {
        return Err(CUSTOM_SERVERS.to_owned());
    }
    if !servers.editable {
        return Err(access_restriction(servers.level, snapshot));
    }
    let server = parse_server(input)?;
    let shown = crate::directory_plain_text(&server, 260);
    let listed = blocks(&servers.denied, &server);
    if block && listed {
        return Err(format!("{shown} is already blocked."));
    }
    if !block && !listed {
        return Err(format!("{shown} is not blocked."));
    }
    if block && servers.denied.len() >= MAX_BLOCKED_SERVERS {
        return Err(format!(
            "This room already blocks {MAX_BLOCKED_SERVERS} servers, the most Morpheus edits."
        ));
    }
    let own = snapshot.members.iter().find(|member| member.is_self);
    let own_server = own.and_then(|member| member.user_id.split_once(':'));
    if block && own_server.is_some_and(|(_, own)| own.eq_ignore_ascii_case(&server)) {
        return Err(OWN_SERVER.to_owned());
    }
    Ok(Intent {
        action: AdminAction::SetAccess(AccessChange::Servers { block }),
        user_id: server,
        name: "Blocked servers".to_owned(),
        expected: MemberExpectation {
            membership: None,
            power: None,
        },
        reason: None,
        knock: false,
    })
}

/// A room or homeserver version, shown as plain bounded text.
fn version_text(version: &str) -> String {
    crate::directory_plain_text(version, 32)
}

fn upgrade_row(upgrade: &UpgradeSettings, snapshot: &RoomAdminSnapshot) -> AccessRow {
    let current = version_text(&snapshot.room_version);
    let recommended = version_text(&upgrade.recommended);
    let (choice, summary) = if upgrade.replaced {
        (2, format!("Version {current}, replaced by a newer room"))
    } else if upgrade.newer {
        (0, format!("Version {current}; version {recommended} is recommended"))
    } else {
        (1, format!("Version {current}, the recommended version"))
    };
    AccessRow {
        kind: ACCESS_UPGRADE,
        choice,
        label: "Room version",
        description: "The version of the rules this room follows. Upgrading replaces the room \
            with a new one on a newer version.",
        summary,
        editable: upgrade.allowed,
        restriction: upgrade_lock(upgrade, snapshot),
    }
}

/// Why the room cannot be upgraded now, or empty when it can.
fn upgrade_lock(upgrade: &UpgradeSettings, snapshot: &RoomAdminSnapshot) -> String {
    if upgrade.allowed {
        String::new()
    } else if upgrade.replaced {
        UPGRADE_REPLACED.to_owned()
    } else if !upgrade.newer {
        UPGRADE_CURRENT.to_owned()
    } else {
        access_restriction(upgrade.level, snapshot)
    }
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
        AccessChange::Encrypt => !access.encrypted,
        AccessChange::Publish { publish } => access.listed == Some(!publish),
        // Checked against the snapshot's address settings instead.
        AccessChange::Address | AccessChange::ClearAddress => true,
        // Checked against the snapshot's version settings or block list instead.
        AccessChange::Upgrade | AccessChange::Servers { .. } => true,
    }
}

/// Change history visibility (`ACCESS_HISTORY`, choice indexes
/// `HISTORY_CHOICES`), guest access (`ACCESS_GUESTS`, choice 1 allows) or
/// turn on encryption (`ACCESS_ENCRYPTION`, choice 1).
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
        ACCESS_ENCRYPTION => {
            if access.encrypted {
                return Err(ENCRYPTION_ON.to_owned());
            }
            if !access.encryption_editable {
                return Err(access_restriction(access.encryption_level, snapshot));
            }
            if choice != 1 {
                return Err("Encryption is already off.".to_owned());
            }
            (AccessChange::Encrypt, "End-to-end encryption")
        }
        ACCESS_DIRECTORY => {
            let listed = access.listed.ok_or_else(|| LISTING_UNKNOWN.to_owned())?;
            if !access.listing_editable {
                return Err(access_restriction(access.listing_level, snapshot));
            }
            let publish = choice == 1;
            if publish == listed {
                return Err("The room directory already shows it that way.".to_owned());
            }
            (AccessChange::Publish { publish }, "Room directory")
        }
        ACCESS_ADDRESS => {
            let addresses = snapshot.addresses.as_ref();
            let addresses = addresses.ok_or_else(|| UNKNOWN_RULES.to_owned())?;
            if !addresses.known {
                return Err(CUSTOM_ADDRESS.to_owned());
            }
            if !addresses.editable {
                return Err(access_restriction(addresses.level, snapshot));
            }
            if addresses.main.is_none() || choice != 0 {
                return Err("The room has no main address to remove.".to_owned());
            }
            (AccessChange::ClearAddress, "Main address")
        }
        ACCESS_UPGRADE => {
            let upgrade = snapshot.upgrade.as_ref();
            let upgrade = upgrade.ok_or_else(|| UNKNOWN_RULES.to_owned())?;
            if !upgrade.allowed {
                return Err(upgrade_lock(upgrade, snapshot));
            }
            (AccessChange::Upgrade, "Room version")
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
        if let AdminAction::SetAccess(AccessChange::Servers { block }) = self.action {
            let Some(servers) = &snapshot.servers else {
                return false;
            };
            return servers.editable && blocks(&servers.denied, &self.user_id) != block;
        }
        if self.action == AdminAction::SetAccess(AccessChange::ClearAddress) {
            let Some(addresses) = &snapshot.addresses else {
                return false;
            };
            return addresses.editable && addresses.main.is_some();
        }
        if self.action == AdminAction::SetAccess(AccessChange::Address) {
            let Some(addresses) = &snapshot.addresses else {
                return false;
            };
            let main = addresses.main.as_deref();
            return addresses.editable && main != Some(self.user_id.as_str());
        }
        if let AdminAction::SetAccess(AccessChange::Upgrade) = self.action {
            let upgrade = snapshot.upgrade.as_ref();
            return upgrade.is_some_and(|upgrade| upgrade.allowed);
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
        REMOVE_MESSAGES if actions.remove_messages => AdminAction::RemoveMessages,
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
    let user_id = shaped_user_id(input)?;
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

/// Ban by Matrix user ID, including someone who already left or never
/// joined.
pub(crate) fn ban_intent(input: &str, snapshot: &RoomAdminSnapshot) -> Result<Intent, String> {
    by_id_intent(AdminAction::Ban, input, snapshot)
}

/// Remove the recent messages of a Matrix user ID, including someone who
/// already left.
pub(crate) fn remove_intent(input: &str, snapshot: &RoomAdminSnapshot) -> Result<Intent, String> {
    by_id_intent(AdminAction::RemoveMessages, input, snapshot)
}

/// The account may remove other members' messages under the snapshot's
/// rules; the session checks the exact rule before sending.
pub(crate) fn can_remove_by_id(snapshot: &RoomAdminSnapshot) -> bool {
    let Some(levels) = snapshot.thresholds else {
        return false;
    };
    snapshot.own_power >= MemberPower::Level(levels.redact)
}

/// A ban or message removal by typed Matrix user ID. A listed member is
/// reviewed as their row shows them, with the member card's refusals; anyone
/// else is reviewed as not in the room. The session still checks the
/// server's current membership and levels before sending.
fn by_id_intent(
    action: AdminAction,
    input: &str,
    snapshot: &RoomAdminSnapshot,
) -> Result<Intent, String> {
    let user_id = shaped_user_id(input)?;
    let banning = action == AdminAction::Ban;
    let allowed = if banning {
        snapshot.own.ban
    } else {
        can_remove_by_id(snapshot)
    };
    if !allowed {
        return Err(permission_text(action, snapshot));
    }
    let permits = |member: &AdminMember| {
        if banning {
            member.actions.ban
        } else {
            member.actions.remove_messages
        }
    };
    let existing = snapshot.member(user_id);
    let name = existing.map_or_else(|| crate::directory_plain_text(user_id, 260), display_name);
    let expected = match existing {
        Some(member) if member.is_self && banning => return Err(LEAVE_INSTEAD.to_owned()),
        Some(member) if member.is_self => return Err(OWN_MESSAGES.to_owned()),
        Some(member) if banning && member.membership == AdminMembership::Ban => {
            return Err(format!("{name} is already banned."));
        }
        Some(member) if member.power == MemberPower::Creator => return Err(CREATOR.to_owned()),
        Some(member) if !permits(member) => {
            return Err(permission_text(action, snapshot));
        }
        Some(member) => MemberExpectation {
            membership: Some(member.membership),
            power: Some(member.power),
        },
        None => MemberExpectation {
            membership: None,
            power: None,
        },
    };
    Ok(Intent {
        action,
        user_id: user_id.to_owned(),
        name,
        expected,
        reason: None,
        knock: false,
    })
}

/// A full Matrix user ID as typed, trimmed; the session parses it exactly.
fn shaped_user_id(input: &str) -> Result<&str, String> {
    let user_id = input.trim();
    let shaped = user_id.len() <= MAX_USER_ID_BYTES
        && user_id.starts_with('@')
        && user_id.contains(':')
        && !user_id.chars().any(char::is_whitespace)
        && !user_id.chars().any(char::is_control);
    if shaped {
        Ok(user_id)
    } else {
        Err(BAD_USER_ID.to_owned())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Confirmation {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) action: &'static str,
    pub(crate) destructive: bool,
}

/// Kick, ban, unban, denying a request, removing messages, every level change
/// and every permission change need a deliberate confirmation. Invitations
/// and approving a request do not.
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
        AdminAction::Kick if revokes_invite(intent) => Confirmation {
            title: format!("Revoke {name}'s invitation?"),
            body: REVOKE_BODY.to_owned(),
            action: "Revoke invitation",
            destructive: true,
        },
        AdminAction::Kick => Confirmation {
            title: format!("Kick {name}?"),
            body: KICK_BODY.to_owned(),
            action: "Kick",
            destructive: true,
        },
        AdminAction::Ban => {
            let absent = intent.expected.membership.is_none();
            let body = if absent { BAN_ABSENT_BODY } else { BAN_BODY };
            Confirmation {
                title: format!("Ban {name}?"),
                body: body.to_owned(),
                action: "Ban",
                destructive: true,
            }
        }
        AdminAction::Unban => Confirmation {
            title: format!("Unban {name}?"),
            body: UNBAN_BODY.to_owned(),
            action: "Unban",
            destructive: false,
        },
        AdminAction::RemoveMessages => Confirmation {
            title: format!("Remove {name}'s recent messages?"),
            body: format!("Up to their {MAX_REMOVED_MESSAGES} latest messages here, {REMOVE_BODY}"),
            action: "Remove messages",
            destructive: true,
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
        AdminAction::SetAccess(AccessChange::Encrypt) => Confirmation {
            title: "Turn on end-to-end encryption?".to_owned(),
            body: ENCRYPT_BODY.to_owned(),
            action: "Turn on encryption",
            destructive: true,
        },
        AdminAction::SetAccess(AccessChange::Address) => {
            let alias = crate::directory_plain_text(&intent.user_id, 260);
            let previous = snapshot.addresses.as_ref().and_then(|a| a.main.as_deref());
            let mut body = format!(
                "If your homeserver has no {alias} yet, Morpheus creates it there for this room. \
                 Everyone sees it as the room's address."
            );
            if let Some(previous) = previous {
                let previous = crate::directory_plain_text(previous, 260);
                body.push_str(&format!(" {previous} keeps working but is no longer the main one."));
            }
            Confirmation {
                title: format!("Make {alias} the main address?"),
                body,
                action: "Set main address",
                destructive: false,
            }
        }
        AdminAction::SetAccess(AccessChange::ClearAddress) => {
            let main = snapshot.addresses.as_ref().and_then(|a| a.main.as_deref());
            let main = crate::directory_plain_text(main.unwrap_or_default(), 260);
            Confirmation {
                title: "Remove the main address?".to_owned(),
                body: format!("{main} keeps working, but the room no longer shows a main address."),
                action: "Remove main address",
                destructive: false,
            }
        }
        AdminAction::SetAccess(AccessChange::Publish { publish: true }) => Confirmation {
            title: "List this room in the room directory?".to_owned(),
            body: LIST_BODY.to_owned(),
            action: "List room",
            destructive: true,
        },
        AdminAction::SetAccess(AccessChange::Publish { publish: false }) => Confirmation {
            title: "Remove this room from the room directory?".to_owned(),
            body: UNLIST_BODY.to_owned(),
            action: "Remove listing",
            destructive: false,
        },
        AdminAction::SetAccess(AccessChange::Servers { block: true }) => {
            let server = crate::directory_plain_text(&intent.user_id, 260);
            Confirmation {
                title: format!("Block {server}?"),
                body: format!("People on {server} {BLOCK_BODY} You can unblock it later."),
                action: "Block server",
                destructive: true,
            }
        }
        AdminAction::SetAccess(AccessChange::Servers { block: false }) => {
            let server = crate::directory_plain_text(&intent.user_id, 260);
            Confirmation {
                title: format!("Unblock {server}?"),
                body: format!("People on {server} can take part in this room again."),
                action: "Unblock server",
                destructive: false,
            }
        }
        AdminAction::SetAccess(AccessChange::Upgrade) => {
            let upgrade = snapshot.upgrade.as_ref();
            let version = upgrade.map_or_else(String::new, |upgrade| {
                version_text(&upgrade.recommended)
            });
            Confirmation {
                title: "Upgrade this room?".to_owned(),
                body: format!(
                    "Your homeserver creates a replacement room on version {version}, copies \
                     this room's name, topic, permissions and addresses, and closes this room \
                     with a pointer to the new one. {UPGRADE_BODY}"
                ),
                action: "Upgrade room",
                destructive: true,
            }
        }
    };
    if let Some(reason) = &intent.reason {
        let body = &mut confirmation.body;
        body.push_str("\n\nReason, visible to the room: ");
        body.push_str(reason);
    }
    Some(confirmation)
}

/// Kicking someone who was only invited withdraws the invitation.
fn revokes_invite(intent: &Intent) -> bool {
    intent.action == AdminAction::Kick
        && intent.expected.membership == Some(AdminMembership::Invite)
}

pub(crate) fn pending_text(intent: &Intent) -> String {
    let verb = match intent.action {
        AdminAction::Invite if intent.knock => "Approving",
        AdminAction::Invite => "Inviting",
        AdminAction::Kick if intent.knock => "Denying",
        AdminAction::Kick if revokes_invite(intent) => "Revoking the invitation of",
        AdminAction::Kick => "Kicking",
        AdminAction::Ban => "Banning",
        AdminAction::Unban => "Unbanning",
        AdminAction::RemoveMessages => "Removing recent messages from",
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
    if let Some(replacement) = &outcome.replacement {
        let id = crate::directory_plain_text(replacement, 260);
        return if outcome.confirmed {
            format!("Upgraded. The replacement room {id} is ready; members need to join it.")
        } else {
            format!(
                "The homeserver created the replacement room {id}, but this room does not \
                 point to it yet. Refresh before trying again."
            )
        };
    }
    if let Some(removal) = outcome.removal {
        return removal_text(name, removal);
    }
    let Some(seen) = outcome.observed else {
        return format!("The homeserver accepted the change for {name}. {UNREAD}");
    };
    if let AdminAction::SetAccess(AccessChange::Servers { block }) = intent.action {
        let server = crate::directory_plain_text(&intent.user_id, 260);
        return match (block, outcome.confirmed) {
            (true, true) => format!("Blocked {server}. The homeserver confirmed it."),
            (false, true) => format!("Unblocked {server}. The homeserver confirmed it."),
            _ => SERVERS_UNCONFIRMED.to_owned(),
        };
    }
    if intent.action == AdminAction::SetAccess(AccessChange::ClearAddress) {
        return if outcome.confirmed {
            "The room no longer shows a main address. The homeserver confirmed it.".to_owned()
        } else {
            ADDRESS_UNCONFIRMED.to_owned()
        };
    }
    if intent.action == AdminAction::SetAccess(AccessChange::Address) {
        let alias = crate::directory_plain_text(&intent.user_id, 260);
        return if outcome.confirmed {
            format!("{alias} is now the main address. The homeserver confirmed it.")
        } else {
            ADDRESS_UNCONFIRMED.to_owned()
        };
    }
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
        AdminAction::Kick if revokes_invite(intent) => {
            format!("Revoked {name}'s invitation. The homeserver confirmed it.")
        }
        AdminAction::Kick => format!("Kicked {name}. The homeserver confirmed they left."),
        AdminAction::Ban => format!("Banned {name}. The homeserver confirmed the ban."),
        AdminAction::Unban => format!("Unbanned {name}. The homeserver confirmed it."),
        AdminAction::RemoveMessages => format!("Removed {name}'s recent messages."),
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

/// What a removal did: only what the homeserver accepted and what reading
/// the same events back showed.
fn removal_text(name: &str, removal: MessageRemoval) -> String {
    let MessageRemoval {
        found,
        removed,
        verified,
        more,
        stopped,
    } = removal;
    if found == 0 && more {
        return format!("{name}'s latest messages here are already removed.");
    }
    if found == 0 {
        return format!("{name} has no recent messages here to remove.");
    }
    let mut text = if removed == found {
        format!("Removed {} from {name}.", messages_text(removed))
    } else {
        format!("Removed {removed} of {} from {name}.", messages_text(found))
    };
    let next = match stopped {
        None => "",
        Some(RoomAdminError::RateLimited) => {
            " The homeserver is limiting requests; wait, then remove again to continue."
        }
        Some(RoomAdminError::TimedOut) => " Remove again to continue.",
        Some(RoomAdminError::Forbidden) => " The homeserver refused the next one; the rest stay.",
        Some(_) => " The next one failed, so the rest stay; remove again to continue.",
    };
    text.push_str(next);
    let check = match verified {
        None => " Reading them back failed; check the conversation.".to_owned(),
        Some(seen) if seen < removed => {
            let left = removed - seen;
            format!(" {left} still show as sent; check the conversation.")
        }
        Some(_) => String::new(),
    };
    text.push_str(&check);
    if more && stopped.is_none() {
        text.push_str(" They may have older messages; remove again to reach them.");
    }
    text
}

fn messages_text(count: usize) -> String {
    if count == 1 {
        "1 recent message".to_owned()
    } else {
        format!("{count} recent messages")
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
        AccessChange::Encrypt => {
            "End-to-end encryption is on. The homeserver confirmed it.".to_owned()
        }
        AccessChange::Upgrade => "The room was upgraded.".to_owned(),
        AccessChange::Servers { .. } => "The blocked servers changed.".to_owned(),
        AccessChange::Publish { publish: true } => {
            "The room is listed in the room directory. The homeserver confirmed it.".to_owned()
        }
        AccessChange::Publish { publish: false } => {
            "The room is no longer listed in the room directory. The homeserver confirmed it."
                .to_owned()
        }
        AccessChange::Address | AccessChange::ClearAddress => {
            "The main address changed.".to_owned()
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
            AccessChange::Encrypt => access.encryption_level,
            AccessChange::Upgrade => snapshot.upgrade.as_ref().map_or(100, |up| up.level),
            AccessChange::Servers { .. } => snapshot.servers.as_ref().map_or(100, |acl| acl.level),
            AccessChange::Publish { .. } => access.listing_level,
            AccessChange::Address | AccessChange::ClearAddress => {
                snapshot.addresses.as_ref().map_or(100, |a| a.level)
            }
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
        AdminAction::RemoveMessages => ("Removing others' messages", levels.redact),
        AdminAction::SetPowerLevel(_)
        | AdminAction::SetPermission(..)
        | AdminAction::SetAccess(_) => ("Changing levels", levels.change_power_levels),
    };
    let any_member = matches!(action, AdminAction::Invite | AdminAction::RemoveMessages);
    if any_member || snapshot.own_power < MemberPower::Level(needed) {
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
    let address = intent.action == AdminAction::SetAccess(AccessChange::Address);
    if address && error == RoomAdminError::NotApplicable {
        return ADDRESS_TAKEN.to_owned();
    }
    if address && error == RoomAdminError::InvalidUserId {
        return "Use a name made of lowercase letters, digits and . _ = -".to_owned();
    }
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
            (RoomAdminError::SelfTarget, _) => return OWN_SERVER.to_owned(),
            (RoomAdminError::InvalidUserId, _) => return BAD_SERVER.to_owned(),
            _ => {}
        }
    }
    let unfinished = error == RoomAdminError::Unavailable || error == RoomAdminError::TimedOut;
    if intent.action == AdminAction::RemoveMessages && unfinished {
        return REMOVE_UNFINISHED.to_owned();
    }
    let fixed = match error {
        RoomAdminError::Cancelled | RoomAdminError::AccountChanged => CLOSED,
        RoomAdminError::NotJoined => "Join this room to manage its members.",
        RoomAdminError::Unavailable | RoomAdminError::TimedOut => UNCONFIRMED,
        RoomAdminError::RateLimited => "The homeserver is limiting requests. Wait, then try again.",
        RoomAdminError::InvalidState => UNREADABLE,
        RoomAdminError::CreatorProtected => CREATOR,
        RoomAdminError::SelfTarget => LEAVE_INSTEAD,
        RoomAdminError::InvalidUserId => BAD_USER_ID,
        RoomAdminError::InvalidPowerLevel => "That power level is outside the range Matrix allows.",
        RoomAdminError::InvalidReason => LONG_REASON,
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
            upgrade: None,
            servers: None,
            addresses: None,
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
        let mut revoke = intent(AdminAction::Kick, 0, false);
        revoke.expected.membership = Some(AdminMembership::Invite);
        let withdrawn = review(&revoke).unwrap();
        assert!(withdrawn.destructive && withdrawn.title == "Revoke Jules's invitation?");
        assert_eq!(withdrawn.action, "Revoke invitation");
        assert!(pending_text(&revoke).starts_with("Revoking the invitation of Jules"));
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
    fn removing_messages_is_confirmed_and_reports_only_what_the_homeserver_did() {
        let mut jules = member("@jules:example.org", None, AdminMembership::Ban);
        let snapshot = roster(vec![jules.clone()], 50);
        assert!(member_intent(REMOVE_MESSAGES, &jules, "", "", &snapshot).is_err());
        jules.actions = MemberActions {
            remove_messages: true,
            ..MemberActions::default()
        };
        let remove = member_intent(REMOVE_MESSAGES, &jules, "", " spam ", &snapshot).unwrap();
        assert_eq!(remove.action, AdminAction::RemoveMessages);
        assert_eq!(remove.reason.as_deref(), Some("spam"));
        assert_eq!(remove.expected.membership, Some(AdminMembership::Ban));
        assert!(remove.still_matches(&snapshot));
        let review = confirmation(&remove, &snapshot).unwrap();
        assert!(review.destructive && review.body.contains("cannot be undone"));
        assert!(review.body.ends_with("Reason, visible to the room: spam"));
        assert!(pending_text(&remove).starts_with("Removing recent messages from"));

        let outcome = |removal: MessageRemoval| AdminOutcome {
            observed: None,
            confirmed: false,
            snapshot: std::sync::Arc::new(snapshot.clone()),
            replacement: None,
            removal: Some(removal),
        };
        let text = |removal| outcome_text(&remove, &outcome(removal));
        let name = &remove.name;
        let all = MessageRemoval {
            found: 3,
            removed: 3,
            verified: Some(3),
            ..MessageRemoval::default()
        };
        assert_eq!(text(all), format!("Removed 3 recent messages from {name}."));
        let limited = text(MessageRemoval {
            found: 40,
            removed: 12,
            verified: Some(12),
            more: true,
            stopped: Some(RoomAdminError::RateLimited),
        });
        assert!(limited.starts_with("Removed 12 of 40 recent messages"));
        assert!(limited.contains("limiting requests") && !limited.contains("older"));
        let unread = MessageRemoval {
            found: 1,
            removed: 1,
            ..MessageRemoval::default()
        };
        assert!(text(unread).contains("Reading them back failed"));
        let lagging = MessageRemoval {
            found: 2,
            removed: 2,
            verified: Some(1),
            ..MessageRemoval::default()
        };
        assert!(text(lagging).contains("1 still show as sent"));
        assert!(text(MessageRemoval::default()).contains("no recent messages"));
        // A failure that may have removed some never reads as a success.
        let failed = error_text(RoomAdminError::Unavailable, &remove, Some(&snapshot));
        assert_eq!(failed, REMOVE_UNFINISHED);
        let mut member_level = snapshot.clone();
        member_level.own_power = MemberPower::Level(0);
        assert_eq!(
            permission_text(AdminAction::RemoveMessages, &member_level),
            "Removing others' messages needs level 50; you have level 0. Ask someone with a \
             higher level."
        );
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
    fn bans_by_id_reach_people_outside_the_room_and_need_ban_permission() {
        let mut joined = member("@in:example.org", None, AdminMembership::Join);
        joined.actions.ban = true;
        let mut own = member("@me:example.org", None, AdminMembership::Join);
        own.is_self = true;
        let members = vec![
            joined,
            own,
            member("@banned:example.org", None, AdminMembership::Ban),
            member("@peer:example.org", None, AdminMembership::Join),
        ];
        let mut snapshot = roster(members, 50);
        let refused = [
            "@me:example.org",
            "@banned:example.org",
            "@peer:example.org",
            "name",
        ];
        for refused in refused {
            assert!(ban_intent(refused, &snapshot).is_err());
        }
        let gone = ban_intent(" @spam:example.org ", &snapshot).unwrap();
        assert_eq!(gone.action, AdminAction::Ban);
        assert_eq!(gone.user_id, "@spam:example.org");
        assert_eq!(gone.expected.membership, None);
        assert_eq!(gone.expected.power, None);
        assert!(gone.still_matches(&snapshot));
        let review = confirmation(&gone, &snapshot).unwrap();
        assert!(review.destructive && review.body.starts_with("They are not in the room now."));
        let listed = ban_intent("@in:example.org", &snapshot).unwrap();
        assert_eq!(listed.expected.membership, Some(AdminMembership::Join));
        assert_eq!(confirmation(&listed, &snapshot).unwrap().body, BAN_BODY);
        snapshot.own.ban = false;
        assert!(ban_intent("@spam:example.org", &snapshot).is_err());
    }

    #[test]
    fn removing_messages_by_id_reaches_people_who_left_at_the_redaction_level() {
        let mut own = member("@me:example.org", None, AdminMembership::Join);
        own.is_self = true;
        let peer = member("@peer:example.org", None, AdminMembership::Join);
        let mut snapshot = roster(vec![own, peer], 50);
        assert!(can_remove_by_id(&snapshot));
        let yours = remove_intent("@me:example.org", &snapshot);
        assert_eq!(yours, Err(OWN_MESSAGES.to_owned()));
        // A listed member needs the member card's permission.
        assert!(remove_intent("@peer:example.org", &snapshot).is_err());
        let gone = remove_intent(" @spam:example.org ", &snapshot).unwrap();
        assert_eq!(gone.action, AdminAction::RemoveMessages);
        assert_eq!(gone.user_id, "@spam:example.org");
        assert_eq!(gone.expected.membership, None);
        assert!(confirmation(&gone, &snapshot).unwrap().destructive);
        snapshot.own_power = MemberPower::Level(0);
        assert!(!can_remove_by_id(&snapshot));
        let low = remove_intent("@spam:example.org", &snapshot).unwrap_err();
        assert!(low.starts_with("Removing others' messages needs level 50"));
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
            encrypted: false,
            encryption_level: 50,
            encryption_editable: editable,
            listed: Some(false),
            listing_level: 50,
            listing_editable: editable,
        });
        snapshot
    }

    #[test]
    fn blocking_servers_is_reviewed_bounded_and_spares_the_own_homeserver() {
        let mut snapshot = with_access(true);
        let mut own = member("@me:example.org", None, AdminMembership::Join);
        own.is_self = true;
        snapshot.members.push(own);
        let unknown = server_intent("spam.example", true, &snapshot);
        assert_eq!(unknown, Err(UNKNOWN_RULES.to_owned()));
        snapshot.servers = Some(ServerAclSettings {
            denied: vec!["Old.Example".to_owned()],
            known: true,
            level: 50,
            editable: true,
        });
        let row = access_rows(&snapshot).pop().unwrap();
        assert_eq!(
            (row.kind, row.choice, row.editable),
            (ACCESS_SERVERS, 1, true)
        );
        assert_eq!(row.summary, "1 server blocked: Old.Example");

        let block = server_intent(" Spam.Example ", true, &snapshot).unwrap();
        let blocking = AccessChange::Servers { block: true };
        assert_eq!(block.action, AdminAction::SetAccess(blocking));
        assert_eq!(block.user_id, "spam.example");
        assert!(block.still_matches(&snapshot));
        let review = confirmation(&block, &snapshot).unwrap();
        assert!(review.destructive && review.title == "Block spam.example?");
        let again = server_intent("old.example", true, &snapshot);
        assert_eq!(again, Err("old.example is already blocked.".to_owned()));
        assert!(server_intent("spam.example", false, &snapshot).is_err());
        let unblock = server_intent("old.example", false, &snapshot).unwrap();
        assert!(!confirmation(&unblock, &snapshot).unwrap().destructive);
        // The own homeserver, ports and spaces are refused before review.
        for refused in ["Example.org", "spam.example:8448", "spam example", ""] {
            assert!(server_intent(refused, true, &snapshot).is_err());
        }
        // A pattern matching the own homeserver is refused by the session.
        let own_pattern = error_text(RoomAdminError::SelfTarget, &block, Some(&snapshot));
        assert_eq!(own_pattern, OWN_SERVER);

        let outcome = AdminOutcome {
            observed: Some(ObservedTarget {
                membership: ObservedMembership::None,
                power: MemberPower::Level(0),
            }),
            confirmed: true,
            snapshot: std::sync::Arc::new(snapshot.clone()),
            replacement: None,
            removal: None,
        };
        let text = outcome_text(&block, &outcome);
        assert_eq!(text, "Blocked spam.example. The homeserver confirmed it.");
        // A review is withdrawn once the server is blocked anyway.
        if let Some(servers) = snapshot.servers.as_mut() {
            servers.denied.push("spam.example".to_owned());
        }
        assert!(!block.still_matches(&snapshot));
        // Allow lists and other settings stay read-only, with the reason.
        if let Some(servers) = snapshot.servers.as_mut() {
            servers.known = false;
            servers.editable = false;
        }
        let custom = server_intent("other.example", true, &snapshot);
        assert_eq!(custom, Err(CUSTOM_SERVERS.to_owned()));
        let row = access_rows(&snapshot).pop().unwrap();
        assert_eq!(row.restriction, CUSTOM_SERVERS);
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
        assert_eq!(rows.len(), 4);
        assert_eq!((rows[0].choice, rows[1].choice, rows[2].choice), (0, 0, 0));
        assert_eq!((rows[3].kind, rows[3].choice), (ACCESS_DIRECTORY, 0));
        let unlocked = |row: &AccessRow| row.editable && row.restriction.is_empty();
        assert!(rows.iter().all(unlocked));
    }

    #[test]
    fn main_addresses_are_reviewed_on_the_own_server_and_taken_ones_explained() {
        let mut snapshot = with_access(true);
        let unknown = address_intent("lounge", &snapshot);
        assert_eq!(unknown, Err(UNKNOWN_RULES.to_owned()));
        snapshot.addresses = Some(AddressSettings {
            main: Some("#old:example.org".to_owned()),
            alternatives: vec!["#x:example.org".to_owned()],
            known: true,
            level: 50,
            editable: true,
            server: "example.org".to_owned(),
        });
        let row = access_rows(&snapshot).pop().unwrap();
        assert_eq!(row.kind, ACCESS_ADDRESS);
        assert_eq!(row.summary, "#old:example.org · 1 other address");
        let set = address_intent(" Lounge ", &snapshot).unwrap();
        assert_eq!(set.user_id, "#lounge:example.org");
        assert!(set.still_matches(&snapshot));
        let review = confirmation(&set, &snapshot).unwrap();
        assert!(!review.destructive && review.body.contains("#old:example.org keeps working"));
        for refused in ["#lounge:other.org", "two words", "#old:example.org", ""] {
            assert!(address_intent(refused, &snapshot).is_err());
        }
        let taken = error_text(RoomAdminError::NotApplicable, &set, Some(&snapshot));
        assert_eq!(taken, ADDRESS_TAKEN);
        // Set meanwhile: the review is withdrawn.
        if let Some(addresses) = snapshot.addresses.as_mut() {
            addresses.main = Some("#lounge:example.org".to_owned());
        }
        assert!(!set.still_matches(&snapshot));
        if let Some(addresses) = snapshot.addresses.as_mut() {
            addresses.known = false;
            addresses.editable = false;
        }
        let custom = address_intent("lounge", &snapshot);
        assert_eq!(custom, Err(CUSTOM_ADDRESS.to_owned()));
        // Removing the main address keeps the address itself working.
        if let Some(addresses) = snapshot.addresses.as_mut() {
            addresses.known = true;
            addresses.editable = true;
        }
        let clear = access_intent(ACCESS_ADDRESS, 0, &snapshot).unwrap();
        let clearing = AdminAction::SetAccess(AccessChange::ClearAddress);
        assert_eq!(clear.action, clearing);
        assert!(clear.still_matches(&snapshot));
        let review = confirmation(&clear, &snapshot).unwrap();
        assert!(review.body.starts_with("#lounge:example.org keeps working"));
        if let Some(addresses) = snapshot.addresses.as_mut() {
            addresses.main = None;
        }
        assert!(!clear.still_matches(&snapshot));
        assert!(access_intent(ACCESS_ADDRESS, 0, &snapshot).is_err());
    }

    #[test]
    fn directory_listing_is_reviewed_and_unknown_listings_stay_read_only() {
        let mut snapshot = with_access(true);
        let list = access_intent(ACCESS_DIRECTORY, 1, &snapshot).unwrap();
        let publish = AccessChange::Publish { publish: true };
        assert_eq!(list.action, AdminAction::SetAccess(publish));
        assert!(list.still_matches(&snapshot));
        let review = confirmation(&list, &snapshot).unwrap();
        assert!(review.destructive && review.body.contains("Who can join is unchanged"));
        let already = access_intent(ACCESS_DIRECTORY, 0, &snapshot);
        assert!(already.is_err());
        let outcome = AdminOutcome {
            observed: Some(ObservedTarget {
                membership: ObservedMembership::None,
                power: MemberPower::Level(0),
            }),
            confirmed: true,
            snapshot: std::sync::Arc::new(snapshot.clone()),
            replacement: None,
            removal: None,
        };
        let text = outcome_text(&list, &outcome);
        assert!(text.starts_with("The room is listed in the room directory."));
        if let Some(access) = snapshot.access.as_mut() {
            access.listed = Some(true);
        }
        // Listed meanwhile: the review is withdrawn, and removal is offered.
        assert!(!list.still_matches(&snapshot));
        let unlist = access_intent(ACCESS_DIRECTORY, 0, &snapshot).unwrap();
        assert!(!confirmation(&unlist, &snapshot).unwrap().destructive);
        if let Some(access) = snapshot.access.as_mut() {
            access.listed = None;
            access.listing_editable = false;
        }
        let unknown = access_intent(ACCESS_DIRECTORY, 1, &snapshot);
        assert_eq!(unknown, Err(LISTING_UNKNOWN.to_owned()));
        let row = &access_rows(&snapshot)[3];
        assert_eq!(row.choice, -1);
        assert_eq!(row.restriction, LISTING_UNKNOWN);
    }

    #[test]
    fn upgrades_are_offered_only_for_a_newer_version_and_reported_with_the_new_room() {
        let mut snapshot = with_access(true);
        snapshot.room_version = "10".to_owned();
        snapshot.upgrade = Some(UpgradeSettings {
            recommended: "11".to_owned(),
            newer: true,
            replaced: false,
            level: 100,
            allowed: true,
        });
        let row = access_rows(&snapshot).pop().unwrap();
        assert_eq!(
            (row.kind, row.choice, row.editable),
            (ACCESS_UPGRADE, 0, true)
        );
        assert_eq!(row.summary, "Version 10; version 11 is recommended");
        let upgrade = access_intent(ACCESS_UPGRADE, 1, &snapshot).unwrap();
        assert_eq!(
            upgrade.action,
            AdminAction::SetAccess(AccessChange::Upgrade)
        );
        let review = confirmation(&upgrade, &snapshot).unwrap();
        assert!(review.destructive && review.body.contains("version 11"));
        assert!(upgrade.still_matches(&snapshot));

        let outcome = AdminOutcome {
            observed: None,
            confirmed: true,
            snapshot: std::sync::Arc::new(snapshot.clone()),
            replacement: Some("!new:example.org".to_owned()),
            removal: None,
        };
        let text = outcome_text(&upgrade, &outcome);
        assert!(text.starts_with("Upgraded.") && text.contains("!new:example.org"));

        // Already replaced, or already current: nothing to do, and a stale
        // review is withdrawn.
        if let Some(settings) = snapshot.upgrade.as_mut() {
            settings.replaced = true;
            settings.allowed = false;
        }
        assert!(!upgrade.still_matches(&snapshot));
        let replaced = access_intent(ACCESS_UPGRADE, 1, &snapshot);
        assert_eq!(replaced, Err(UPGRADE_REPLACED.to_owned()));
        let row = access_rows(&snapshot).pop().unwrap();
        assert_eq!((row.choice, row.editable), (2, false));
    }

    #[test]
    fn encryption_only_turns_on_after_a_destructive_review() {
        let snapshot = with_access(true);
        let encrypt = access_intent(ACCESS_ENCRYPTION, 1, &snapshot).unwrap();
        assert_eq!(
            encrypt.action,
            AdminAction::SetAccess(AccessChange::Encrypt)
        );
        let review = confirmation(&encrypt, &snapshot).unwrap();
        assert!(review.destructive && review.body.contains("cannot be turned off"));
        assert!(encrypt.still_matches(&snapshot));
        assert!(access_intent(ACCESS_ENCRYPTION, 0, &snapshot).is_err());
        let locked = access_intent(ACCESS_ENCRYPTION, 1, &with_access(false));
        assert!(locked.is_err_and(|text| text.contains("needs level 50")));

        // Once on, it stays on: no action, and a stale review is withdrawn.
        let mut on = snapshot.clone();
        if let Some(access) = on.access.as_mut() {
            access.encrypted = true;
            access.encryption_editable = false;
        }
        assert!(!encrypt.still_matches(&on));
        assert_eq!(
            access_intent(ACCESS_ENCRYPTION, 1, &on),
            Err(ENCRYPTION_ON.to_owned())
        );
        let row = &access_rows(&on)[2];
        assert_eq!(
            (row.choice, row.summary.as_str(), row.editable),
            (1, "On", false)
        );
        assert_eq!(row.restriction, ENCRYPTION_ON);
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
