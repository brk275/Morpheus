//! Live assertions share the moderation fixture's disposable room and devices.
//! Fixed evidence is emitted only after independent homeserver observations.

use matrix_sdk::{
    Client,
    ruma::{
        RoomId,
        api::client::{directory::get_room_visibility, room::Visibility},
        events::StateEventType,
    },
};
use morpheus_matrix::{
    AccountManager, AvatarPng, CurrentJoinRule, EditableJoinRule, JoinRuleKind, RoomMembership,
    RoomSettingsCancellation, RoomSettingsError,
};

use super::{read_state, wait_for_room};

// Original public 1×1 RGBA image. There is no private media or generated file.
const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 0,
    5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn image() -> AvatarPng {
    AvatarPng::new(PNG.to_vec(), 1, 1).expect("Invalid original avatar fixture")
}

async fn private_directory(client: &Client, room_id: &str) {
    let request = get_room_visibility::v3::Request::new(
        RoomId::parse(room_id).expect("Invalid disposable room ID"),
    );
    let response = client
        .send(request)
        .await
        .expect("Cannot read directory visibility");
    assert!(matches!(response.visibility, Visibility::Private));
}

pub(super) async fn exercise(
    creator: &AccountManager,
    peer: &AccountManager,
    creator_id: &str,
    peer_id: &str,
    client: &Client,
    room_id: &str,
) {
    eprintln!("MORPHEUS_MODERATION_STAGE room_settings");
    let selection = creator
        .account_selection(creator_id)
        .expect("Creator selection missing");
    let peer_selection = peer
        .account_selection(peer_id)
        .expect("Peer selection missing");
    assert!(matches!(
        creator
            .open_room_settings_for(&peer_selection, room_id, RoomSettingsCancellation::new())
            .await,
        Err(RoomSettingsError::AccountChanged)
    ));
    let cancellation = RoomSettingsCancellation::new();
    let settings = creator
        .open_room_settings_for(&selection, room_id, cancellation.clone())
        .await
        .expect("Cannot open creator room settings");
    let stale = creator
        .open_room_settings_for(&selection, room_id, RoomSettingsCancellation::new())
        .await
        .expect("Cannot open concurrent room settings");
    let peer_settings = peer
        .open_room_settings_for(&peer_selection, room_id, RoomSettingsCancellation::new())
        .await
        .expect("Cannot open member room settings");
    let initial = settings.snapshot();
    assert!(initial.encrypted && initial.permissions.avatar && initial.permissions.join_rule);
    assert!(matches!(
        initial.join_rule,
        CurrentJoinRule::Editable(EditableJoinRule::Invite)
    ));
    assert!(!peer_settings.snapshot().permissions.avatar);
    assert!(!peer_settings.snapshot().permissions.join_rule);
    assert!(matches!(
        peer_settings.set_name("Unauthorized").await,
        Err(RoomSettingsError::PermissionDenied)
    ));
    assert!(matches!(
        peer_settings.set_topic("Unauthorized").await,
        Err(RoomSettingsError::PermissionDenied)
    ));
    assert!(matches!(
        peer_settings.set_avatar(image()).await,
        Err(RoomSettingsError::PermissionDenied)
    ));
    assert!(matches!(
        peer_settings.set_join_rule(EditableJoinRule::Public).await,
        Err(RoomSettingsError::PermissionDenied)
    ));
    let join = read_state(client, room_id, StateEventType::RoomJoinRules, "").await;
    assert!(join["join_rule"] == "invite");
    assert!(
        !settings
            .refresh()
            .await
            .expect("Cannot refresh after rejected avatar")
            .avatar
            .present
    );
    eprintln!("MORPHEUS_MODERATION_CHECK settings_permission_denial=true");

    stale
        .set_topic("Concurrent topic")
        .await
        .expect("Cannot make independent topic edit");
    let named = settings
        .set_name("  Morpheus room details  ")
        .await
        .expect("Cannot save room name through session");
    assert!(named.name == "Morpheus room details");
    assert!(
        named.topic == initial.topic,
        "Saving a name silently replaced the topic baseline"
    );
    assert!(matches!(
        settings.set_topic("Old form topic draft").await,
        Err(RoomSettingsError::BaselineChanged)
    ));
    assert!(
        read_state(client, room_id, StateEventType::RoomTopic, "").await["topic"]
            == "Concurrent topic"
    );
    settings
        .refresh()
        .await
        .expect("Cannot explicitly refresh conflicted topic");
    assert!(
        read_state(client, room_id, StateEventType::RoomName, "").await["name"]
            == "Morpheus room details"
    );
    let topic = settings
        .set_topic("  Room avatar and access checks\n")
        .await
        .expect("Cannot save room topic through session");
    assert!(topic.topic == "  Room avatar and access checks\n");
    assert!(
        read_state(client, room_id, StateEventType::RoomTopic, "").await["topic"]
            == "  Room avatar and access checks\n"
    );
    assert!(matches!(
        stale.set_name("Stale room name").await,
        Err(RoomSettingsError::BaselineChanged)
    ));
    assert!(matches!(
        stale.set_topic("Stale topic").await,
        Err(RoomSettingsError::BaselineChanged)
    ));
    assert!(
        read_state(client, room_id, StateEventType::RoomName, "").await["name"]
            == "Morpheus room details"
    );
    assert!(
        read_state(client, room_id, StateEventType::RoomTopic, "").await["topic"]
            == "  Room avatar and access checks\n"
    );
    let cleared = settings
        .set_topic("")
        .await
        .expect("Cannot clear topic through session");
    assert!(cleared.topic.is_empty());
    assert!(read_state(client, room_id, StateEventType::RoomTopic, "").await["topic"] == "");
    eprintln!("MORPHEUS_MODERATION_CHECK settings_text_roundtrip=true");

    let uploaded = settings
        .set_avatar(image())
        .await
        .expect("Creator avatar upload failed");
    assert!(uploaded.avatar.present && uploaded.avatar.preview_available);
    assert!(uploaded.avatar.width == Some(1) && uploaded.avatar.height == Some(1));
    let avatar = read_state(client, room_id, StateEventType::RoomAvatar, "").await;
    assert!(
        avatar["url"]
            .as_str()
            .is_some_and(|url| url.starts_with("mxc://"))
    );
    assert!(avatar["info"]["mimetype"] == "image/png");
    assert!(avatar["info"]["w"] == 1 && avatar["info"]["h"] == 1);
    assert!(avatar["info"]["size"].as_u64() == Some(PNG.len() as u64));
    assert!(matches!(
        peer_settings.download_avatar_preview(false).await,
        Err(RoomSettingsError::EncryptedPreviewDisabled)
    ));
    let preview = peer_settings
        .download_avatar_preview(true)
        .await
        .expect("Explicit member avatar preview failed")
        .expect("Uploaded avatar missing");
    assert!(
        preview.bytes == PNG,
        "Remote avatar bytes differ from the original fixture"
    );
    eprintln!("MORPHEUS_MODERATION_CHECK settings_avatar_roundtrip=true");
    eprintln!("MORPHEUS_MODERATION_CHECK settings_encrypted_preview_opt_in=true");
    assert!(matches!(
        stale.remove_avatar().await,
        Err(RoomSettingsError::BaselineChanged)
    ));
    let preserved = read_state(client, room_id, StateEventType::RoomAvatar, "").await;
    assert!(
        preserved == avatar,
        "A stale form changed the server avatar"
    );
    let removed = settings
        .remove_avatar()
        .await
        .expect("Creator cannot remove avatar");
    assert!(!removed.avatar.present && !removed.avatar.preview_available);
    let empty = read_state(client, room_id, StateEventType::RoomAvatar, "").await;
    assert!(empty["url"].is_null());
    assert!(
        peer_settings
            .download_avatar_preview(true)
            .await
            .expect("Cannot check removed avatar")
            .is_none()
    );
    eprintln!("MORPHEUS_MODERATION_CHECK settings_avatar_remove=true");

    private_directory(client, room_id).await;
    let changed = settings
        .set_join_rule(EditableJoinRule::Public)
        .await
        .expect("Cannot enable public joining");
    assert!(matches!(
        changed.join_rule,
        CurrentJoinRule::Editable(EditableJoinRule::Public)
    ));
    let public = read_state(client, room_id, StateEventType::RoomJoinRules, "").await;
    assert!(public["join_rule"] == "public");
    private_directory(client, room_id).await;
    assert!(matches!(
        stale.set_join_rule(EditableJoinRule::Invite).await,
        Err(RoomSettingsError::BaselineChanged)
    ));
    assert!(read_state(client, room_id, StateEventType::RoomJoinRules, "").await == public);
    eprintln!("MORPHEUS_MODERATION_CHECK settings_stale_write_rejected=true");

    let space = creator
        .create_space("Morpheus access-rule fixture")
        .await
        .expect("Cannot create disposable allowed Space");
    wait_for_room(creator, &space, RoomMembership::Joined).await;
    for (rule, wire) in [
        (EditableJoinRule::Knock, "knock"),
        (
            EditableJoinRule::Restricted {
                allow_rooms: vec![space.clone()],
            },
            "restricted",
        ),
        (
            EditableJoinRule::KnockRestricted {
                allow_rooms: vec![space.clone()],
            },
            "knock_restricted",
        ),
        (EditableJoinRule::Invite, "invite"),
    ] {
        // The pinned disposable homeserver creates modern rooms. An omitted
        // mode is a fixture failure, not a skipped capability assertion.
        assert!(initial.available_join_rules.contains(&rule.kind()));
        let snapshot = settings
            .set_join_rule(rule.clone())
            .await
            .expect("Cannot save supported access rule");
        assert!(snapshot.join_rule == CurrentJoinRule::Editable(rule.clone()));
        let state = read_state(client, room_id, StateEventType::RoomJoinRules, "").await;
        assert!(state["join_rule"] == wire);
        if matches!(
            rule.kind(),
            JoinRuleKind::Restricted | JoinRuleKind::KnockRestricted
        ) {
            let allow = state["allow"]
                .as_array()
                .expect("Restricted allow list absent");
            assert!(allow.len() == 1);
            assert!(allow[0]["type"] == "m.room_membership");
            assert!(allow[0]["room_id"].as_str() == Some(space.as_str()));
        }
        private_directory(client, room_id).await;
    }
    creator
        .leave_room(&space)
        .await
        .expect("Cannot leave disposable allowed Space");
    eprintln!("MORPHEUS_MODERATION_CHECK settings_access_rules_roundtrip=true");
    eprintln!("MORPHEUS_MODERATION_CHECK settings_directory_unchanged=true");

    cancellation.cancel();
    assert!(matches!(
        settings.set_name("Cancelled name").await,
        Err(RoomSettingsError::Cancelled)
    ));
    assert!(matches!(
        settings.set_topic("Cancelled topic").await,
        Err(RoomSettingsError::Cancelled)
    ));
    assert!(matches!(
        settings.refresh().await,
        Err(RoomSettingsError::Cancelled)
    ));
    assert!(matches!(
        settings.set_avatar(image()).await,
        Err(RoomSettingsError::Cancelled)
    ));
    assert!(matches!(
        settings.remove_avatar().await,
        Err(RoomSettingsError::Cancelled)
    ));
    assert!(matches!(
        settings.set_join_rule(EditableJoinRule::Public).await,
        Err(RoomSettingsError::Cancelled)
    ));
    assert!(matches!(
        settings.download_avatar_preview(true).await,
        Err(RoomSettingsError::Cancelled)
    ));
    assert!(read_state(client, room_id, StateEventType::RoomAvatar, "").await == empty);
    assert!(
        read_state(client, room_id, StateEventType::RoomJoinRules, "").await["join_rule"]
            == "invite"
    );
    assert!(
        read_state(client, room_id, StateEventType::RoomName, "").await["name"]
            == "Morpheus room details"
    );
    assert!(read_state(client, room_id, StateEventType::RoomTopic, "").await["topic"] == "");
    eprintln!("MORPHEUS_MODERATION_CHECK settings_cancelled_form_rejected=true");
}
