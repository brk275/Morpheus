//! Current local trust and notification policy for an incoming invitation.
//!
//! A receipt's `EncryptionInfo::Verified` authenticates its original sender;
//! it is not a current permission to ring or join. The pinned SDK preserves
//! historical session trust, even through `Room::get_encryption_info`. Resolve
//! the device again and bind both original keys before checking current trust.
//!
//! These checks use the active client's current local stores. They do not
//! assert that a remote revocation has already reached this client. In this
//! SDK, `request_user_identity` also hides federation `/keys/query` failures,
//! so its successful return is not proof of a successful fresh remote lookup.
//! The owner must require sync readiness, check both invitation and membership
//! receipts, revalidate on changes and on Accept, and retain account-selection
//! cancellation through joining. No helper creates a client, task or network
//! key refresh. Push evaluation is bounded in case a rule needs extra context.

use std::time::Duration;

use matrix_sdk::{
    Client, Room, RoomState,
    deserialized_responses::{AlgorithmInfo, EncryptionInfo, VerificationState},
    room::PushContext,
    ruma::{
        DeviceId, DeviceKeyAlgorithm, OwnedRoomId, OwnedUserId, RoomId, UserId,
        events::{
            ignored_user_list::IgnoredUserListEventContent, push_rules::PushRulesEventContent,
        },
        power_levels::NotificationPowerLevelsKey,
        push::{Action, PushConditionRoomCtx, Ruleset},
        serde::Raw,
    },
};
use morpheus_rtc::ringing::RoomMentionPermission;
use tokio::time::timeout;

const POLICY_TIMEOUT: Duration = Duration::from_secs(5);

/// Fixed categories only: never retain an SDK error or private event material.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PolicyFailure {
    InvalidMetadata,
    TrustUnavailable,
    UntrustedSender,
    InvalidEvent,
    PolicyUnavailable,
    IgnoredSender,
    NotificationDenied,
    TimedOut,
}

struct OriginalSender<'a> {
    user_id: &'a UserId,
    device_id: &'a DeviceId,
    curve25519: &'a str,
    ed25519: &'a str,
}

impl<'a> OriginalSender<'a> {
    fn from_info(info: &'a EncryptionInfo) -> Result<Self, PolicyFailure> {
        if info.verification_state != VerificationState::Verified {
            return Err(PolicyFailure::InvalidMetadata);
        }
        let AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key,
            sender_claimed_keys,
            session_id: Some(session_id),
        } = &info.algorithm_info
        else {
            return Err(PolicyFailure::InvalidMetadata);
        };
        let ed25519 = sender_claimed_keys
            .get(&DeviceKeyAlgorithm::Ed25519)
            .filter(|key| !key.is_empty())
            .ok_or(PolicyFailure::InvalidMetadata)?;
        if curve25519_key.is_empty() || session_id.is_empty() {
            return Err(PolicyFailure::InvalidMetadata);
        }
        Ok(Self {
            user_id: &info.sender,
            device_id: info
                .sender_device
                .as_deref()
                .ok_or(PolicyFailure::InvalidMetadata)?,
            curve25519: curve25519_key,
            ed25519,
        })
    }

    fn require_current(&self, current: CurrentDeviceFacts<'_>) -> Result<(), PolicyFailure> {
        if current.user_id != self.user_id
            || current.device_id != self.device_id
            || current.curve25519.as_deref() != Some(self.curve25519)
            || current.ed25519.as_deref() != Some(self.ed25519)
            || current.blacklisted
            || current.deleted
            || !current.cross_signing_verified
        {
            return Err(PolicyFailure::UntrustedSender);
        }
        Ok(())
    }
}

// Private facts are produced directly from a newly resolved SDK Device. Keeping
// the predicate separate permits regression tests without a device or service.
struct CurrentDeviceFacts<'a> {
    user_id: &'a UserId,
    device_id: &'a DeviceId,
    curve25519: Option<String>,
    ed25519: Option<String>,
    blacklisted: bool,
    deleted: bool,
    cross_signing_verified: bool,
}

pub(super) async fn require_current_sender(
    client: &Client,
    info: &EncryptionInfo,
) -> Result<(), PolicyFailure> {
    let original = OriginalSender::from_info(info)?;
    timeout(POLICY_TIMEOUT, async {
        let device = client
            .encryption()
            .get_device(original.user_id, original.device_id)
            .await
            .map_err(|_| PolicyFailure::TrustUnavailable)?
            .ok_or(PolicyFailure::UntrustedSender)?;
        original.require_current(CurrentDeviceFacts {
            user_id: device.user_id(),
            device_id: device.device_id(),
            curve25519: device.curve25519_key().map(|key| key.to_base64()),
            ed25519: device.ed25519_key().map(|key| key.to_base64()),
            blacklisted: device.is_blacklisted(),
            deleted: device.is_deleted(),
            // `is_verified()` alone also accepts local trust and does not
            // veto a blacklist. Require the current cross-signing chain.
            cross_signing_verified: device.is_verified_with_cross_signing(),
        })
    })
    .await
    .map_err(|_| PolicyFailure::TimedOut)?
}

fn require_event_binding<T>(
    event: &Raw<T>,
    room_id: &RoomId,
    sender: &UserId,
) -> Result<(), PolicyFailure> {
    let event_sender = event
        .get_field::<OwnedUserId>("sender")
        .map_err(|_| PolicyFailure::InvalidEvent)?;
    let event_room = event
        .get_field::<OwnedRoomId>("room_id")
        .map_err(|_| PolicyFailure::InvalidEvent)?;
    let event_type = event
        .get_field::<String>("type")
        .map_err(|_| PolicyFailure::InvalidEvent)?;
    if event_sender.as_deref() != Some(sender)
        || event_room.as_deref().is_some_and(|room| room != room_id)
        || !matches!(
            event_type.as_deref(),
            Some("m.rtc.notification" | "org.matrix.msc4075.rtc.notification")
        )
    {
        return Err(PolicyFailure::InvalidEvent);
    }
    Ok(())
}

fn require_not_ignored(
    raw: Option<&Raw<IgnoredUserListEventContent>>,
    sender: &UserId,
) -> Result<(), PolicyFailure> {
    // A synced account may legitimately have no ignore-list event. A read or
    // decoding error is different and must not become an empty list.
    let Some(raw) = raw else {
        return Ok(());
    };
    let content = policy_object(raw)?;
    let users = content
        .get("ignored_users")
        .and_then(serde_json::Value::as_object)
        .ok_or(PolicyFailure::PolicyUnavailable)?;
    if users.values().any(|user| !user.is_object()) {
        return Err(PolicyFailure::PolicyUnavailable);
    }
    let ignored = raw
        .deserialize()
        .map_err(|_| PolicyFailure::PolicyUnavailable)?;
    if ignored.ignored_users.contains_key(sender) {
        Err(PolicyFailure::IgnoredSender)
    } else {
        Ok(())
    }
}

fn policy_object<T>(
    raw: &Raw<T>,
) -> Result<serde_json::Map<String, serde_json::Value>, PolicyFailure> {
    // Serde's derived struct visitors also accept positional JSON arrays.
    // Matrix account-data content is an object, including empty policy data.
    serde_json::from_str(raw.json().get()).map_err(|_| PolicyFailure::PolicyUnavailable)
}

fn strict_push_rules(raw: Option<&Raw<PushRulesEventContent>>) -> Result<Ruleset, PolicyFailure> {
    // `Account::push_rules` falls back to server defaults on malformed data.
    // Incoming calls instead wait until actual valid policy is available.
    let raw = raw.ok_or(PolicyFailure::PolicyUnavailable)?;
    let content = policy_object(raw)?;
    let global = content
        .get("global")
        .and_then(serde_json::Value::as_object)
        .ok_or(PolicyFailure::PolicyUnavailable)?;
    // In pinned Ruma, every Ruleset field defaults, so `global: []` becomes
    // an empty ruleset. Individual rules and condition helpers also accept
    // struct sequences. Check their JSON envelopes before typed validation;
    // optional categories/conditions and unknown extensions stay supported.
    for kind in [
        "content",
        "override",
        "room",
        "sender",
        "underride",
        "postcontent",
    ] {
        let Some(rules) = global.get(kind) else {
            continue;
        };
        for rule in rules.as_array().ok_or(PolicyFailure::PolicyUnavailable)? {
            let rule = rule.as_object().ok_or(PolicyFailure::PolicyUnavailable)?;
            if matches!(kind, "override" | "underride" | "postcontent")
                && let Some(conditions) = rule.get("conditions")
                && conditions
                    .as_array()
                    .is_none_or(|conditions| conditions.iter().any(|value| !value.is_object()))
            {
                return Err(PolicyFailure::PolicyUnavailable);
            }
        }
    }
    raw.deserialize()
        .map(|content| content.global)
        .map_err(|_| PolicyFailure::PolicyUnavailable)
}

async fn evaluate_policy<T>(
    context: PushConditionRoomCtx,
    rules: Ruleset,
    sender: &UserId,
    event: &Raw<T>,
) -> Result<RoomMentionPermission, PolicyFailure> {
    let room_mention_allowed = context
        .power_levels
        .as_ref()
        .ok_or(PolicyFailure::PolicyUnavailable)?
        .has_sender_notification_permission(sender, &NotificationPowerLevelsKey::Room);
    let actions = PushContext::new(context, rules).for_event(event).await;
    if !actions.iter().any(Action::should_notify) {
        return Err(PolicyFailure::NotificationDenied);
    }
    Ok(if room_mention_allowed {
        RoomMentionPermission::Allowed
    } else {
        RoomMentionPermission::Denied
    })
}

/// Evaluate the exact decrypted notification, never its encrypted envelope or
/// a reconstructed event with inserted mentions. This permits a banner only;
/// it does not authorize sound, media capture, membership or account switching.
pub(super) async fn require_event_policy<T>(
    room: &Room,
    sender: &UserId,
    event: &Raw<T>,
) -> Result<RoomMentionPermission, PolicyFailure> {
    require_event_binding(event, room.room_id(), sender)?;
    timeout(POLICY_TIMEOUT, async {
        if room.state() != RoomState::Joined || room.client().user_id().is_none() {
            return Err(PolicyFailure::PolicyUnavailable);
        }
        let account = room.client().account();
        let ignored = account
            .account_data::<IgnoredUserListEventContent>()
            .await
            .map_err(|_| PolicyFailure::PolicyUnavailable)?;
        require_not_ignored(ignored.as_ref(), sender)?;
        let rules = account
            .account_data::<PushRulesEventContent>()
            .await
            .map_err(|_| PolicyFailure::PolicyUnavailable)?;
        let rules = strict_push_rules(rules.as_ref())?;
        let context = room
            .push_condition_room_ctx()
            .await
            .map_err(|_| PolicyFailure::PolicyUnavailable)?
            .ok_or(PolicyFailure::PolicyUnavailable)?;
        evaluate_policy(context, rules, sender, event).await
    })
    .await
    .map_err(|_| PolicyFailure::TimedOut)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::{
        deserialized_responses::VerificationLevel,
        ruma::{
            device_id,
            push::PushConditionPowerLevelsCtx,
            room_id,
            room_version_rules::{AuthorizationRules, RoomPowerLevelsRules},
            user_id,
        },
    };
    use serde_json::{Value, json};

    fn info() -> EncryptionInfo {
        EncryptionInfo {
            sender: user_id!("@caller:example.org").to_owned(),
            sender_device: Some(device_id!("CALLER").to_owned()),
            forwarder: None,
            algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                curve25519_key: "original_curve_key".into(),
                sender_claimed_keys: [(DeviceKeyAlgorithm::Ed25519, "original_signing_key".into())]
                    .into(),
                session_id: Some("original_session".into()),
            },
            verification_state: VerificationState::Verified,
        }
    }

    fn current() -> CurrentDeviceFacts<'static> {
        CurrentDeviceFacts {
            user_id: user_id!("@caller:example.org"),
            device_id: device_id!("CALLER"),
            curve25519: Some("original_curve_key".into()),
            ed25519: Some("original_signing_key".into()),
            blacklisted: false,
            deleted: false,
            cross_signing_verified: true,
        }
    }

    #[test]
    fn authenticated_keys_and_current_cross_signing_are_both_required() {
        let info = info();
        let original = OriginalSender::from_info(&info).unwrap();
        assert_eq!(original.require_current(current()), Ok(()));
        for replacement in [
            CurrentDeviceFacts {
                user_id: user_id!("@other:example.org"),
                ..current()
            },
            CurrentDeviceFacts {
                device_id: device_id!("REPLACEMENT"),
                ..current()
            },
            CurrentDeviceFacts {
                curve25519: Some("replacement".into()),
                ..current()
            },
            CurrentDeviceFacts {
                ed25519: Some("replacement".into()),
                ..current()
            },
            CurrentDeviceFacts {
                curve25519: None,
                ..current()
            },
            CurrentDeviceFacts {
                ed25519: None,
                ..current()
            },
        ] {
            assert_eq!(
                original.require_current(replacement),
                Err(PolicyFailure::UntrustedSender)
            );
        }
    }

    #[test]
    fn blacklist_and_deletion_override_retained_verified_metadata() {
        let info = info();
        let original = OriginalSender::from_info(&info).unwrap();
        for denied in [
            CurrentDeviceFacts {
                blacklisted: true,
                ..current()
            },
            CurrentDeviceFacts {
                deleted: true,
                ..current()
            },
        ] {
            assert_eq!(
                original.require_current(denied),
                Err(PolicyFailure::UntrustedSender)
            );
        }
    }

    #[test]
    fn master_or_own_identity_reset_cannot_use_historical_verification() {
        let info = info();
        let original = OriginalSender::from_info(&info).unwrap();
        // The SDK computes this fact from both the current remote master key
        // and our current verified identity. Either reset breaks that chain;
        // a local trust flag does not restore it for this admission policy.
        let after_reset = CurrentDeviceFacts {
            cross_signing_verified: false,
            ..current()
        };
        assert_eq!(
            original.require_current(after_reset),
            Err(PolicyFailure::UntrustedSender)
        );
        assert_eq!(info.verification_state, VerificationState::Verified);
    }

    #[test]
    fn incomplete_authenticated_metadata_is_rejected_before_lookup() {
        let mut unverified = info();
        unverified.verification_state =
            VerificationState::Unverified(VerificationLevel::UnverifiedIdentity);
        assert!(matches!(
            OriginalSender::from_info(&unverified),
            Err(PolicyFailure::InvalidMetadata)
        ));
        let mut missing_device = info();
        missing_device.sender_device = None;
        assert!(matches!(
            OriginalSender::from_info(&missing_device),
            Err(PolicyFailure::InvalidMetadata)
        ));
        for field in ["curve", "signing", "session"] {
            let mut invalid = info();
            let AlgorithmInfo::MegolmV1AesSha2 {
                curve25519_key,
                sender_claimed_keys,
                session_id,
            } = &mut invalid.algorithm_info
            else {
                unreachable!()
            };
            match field {
                "curve" => curve25519_key.clear(),
                "signing" => sender_claimed_keys.clear(),
                _ => *session_id = None,
            }
            assert!(matches!(
                OriginalSender::from_info(&invalid),
                Err(PolicyFailure::InvalidMetadata)
            ));
        }
    }

    fn raw<T>(value: Value) -> Raw<T> {
        Raw::from_json_string(value.to_string()).unwrap()
    }

    fn event(mentions: Value) -> Raw<Value> {
        raw(json!({
            "type": "m.rtc.notification", "sender": "@caller:example.org",
            "event_id": "$ring", "content": {"m.mentions": mentions}
        }))
    }

    fn context(caller_power: i32) -> PushConditionRoomCtx {
        let levels = PushConditionPowerLevelsCtx::new(
            [(
                user_id!("@caller:example.org").to_owned(),
                caller_power.into(),
            )]
            .into(),
            0.into(),
            Default::default(),
            RoomPowerLevelsRules::new(&AuthorizationRules::V11, []),
        );
        PushConditionRoomCtx::new(
            room_id!("!room:example.org").to_owned(),
            2u32.into(),
            user_id!("@recipient:example.org").to_owned(),
            "Recipient".into(),
        )
        .with_power_levels(levels)
    }

    #[test]
    fn ignore_list_errors_never_become_permission() {
        let caller = user_id!("@caller:example.org");
        assert_eq!(require_not_ignored(None, caller), Ok(()));
        assert_eq!(
            require_not_ignored(Some(&raw(json!({"ignored_users": {}}))), caller),
            Ok(())
        );
        assert_eq!(
            require_not_ignored(
                Some(&raw(json!({"ignored_users": {"@caller:example.org": {}}}))),
                caller
            ),
            Err(PolicyFailure::IgnoredSender)
        );
        for malformed in [
            json!({}),
            json!([{}]),
            json!({"ignored_users": []}),
            json!({"ignored_users": {"@other:example.org": []}}),
            json!({"ignored_users": {"invalid": {}}}),
        ] {
            assert_eq!(
                require_not_ignored(Some(&raw(malformed)), caller),
                Err(PolicyFailure::PolicyUnavailable)
            );
        }
        assert_eq!(
            require_not_ignored(
                Some(&raw(json!({
                    "ignored_users": {"@other:example.org": {"com.example.note": []}},
                    "com.example.extension": []
                }))),
                caller
            ),
            Ok(())
        );
    }

    #[test]
    fn missing_or_malformed_push_rules_do_not_use_defaults() {
        assert!(matches!(
            strict_push_rules(None),
            Err(PolicyFailure::PolicyUnavailable)
        ));
        for malformed in [
            json!({}),
            json!([{}]),
            json!(null),
            json!({"global": []}),
            json!({"global": null}),
            json!({"global": {"override": "bad"}}),
        ] {
            assert!(matches!(
                strict_push_rules(Some(&raw(malformed))),
                Err(PolicyFailure::PolicyUnavailable)
            ));
        }
        assert!(strict_push_rules(Some(&raw(json!({"global": {}})))).is_ok());
    }

    #[test]
    fn sequence_shaped_push_rule_entries_are_not_policy() {
        // These positions match the pinned derived struct field order. They
        // must not become rules merely because Serde accepts that encoding.
        for (kind, rule) in [
            ("content", json!([["notify"], false, true, "rule", "*"])),
            ("override", json!([["notify"], false, true, "rule", []])),
            ("underride", json!([["notify"], false, true, "rule", []])),
            ("postcontent", json!([["notify"], false, true, "rule", []])),
            (
                "room",
                json!([["notify"], false, true, "!room:example.org"]),
            ),
            (
                "sender",
                json!([["notify"], false, true, "@caller:example.org"]),
            ),
        ] {
            let mut content = json!({"global": {}});
            content["global"][kind] = json!([rule]);
            assert!(matches!(
                strict_push_rules(Some(&raw(content))),
                Err(PolicyFailure::PolicyUnavailable)
            ));
        }
    }

    #[test]
    fn malformed_nested_push_conditions_and_fields_are_not_policy() {
        let rule = || {
            json!({
                "actions": ["notify"], "default": false, "enabled": true, "rule_id": "rule"
            })
        };
        for conditions in [
            json!(null),
            json!({}),
            json!([null]),
            json!([["contains_display_name"]]),
            json!([{"kind": "event_match", "pattern": "*"}]),
        ] {
            let mut malformed = rule();
            malformed["conditions"] = conditions;
            assert!(matches!(
                strict_push_rules(Some(&raw(json!({"global": {"override": [malformed]}})))),
                Err(PolicyFailure::PolicyUnavailable)
            ));
        }
        // Shape validation supplements Ruma; it must not replace validation
        // of required fields or accept malformed action values.
        for (field, value) in [("actions", json!([[]])), ("enabled", json!("true"))] {
            let mut malformed = rule();
            malformed[field] = value;
            assert!(matches!(
                strict_push_rules(Some(&raw(json!({"global": {"override": [malformed]}})))),
                Err(PolicyFailure::PolicyUnavailable)
            ));
        }
    }

    #[test]
    fn valid_serialized_rules_and_optional_extensions_remain_policy() {
        let defaults = Ruleset::server_default(user_id!("@recipient:example.org"));
        let expected = serde_json::to_value(&defaults).unwrap();
        let parsed = strict_push_rules(Some(&raw(json!({"global": expected.clone()})))).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), expected);

        // Empty/omitted categories and omitted conditions are legitimate.
        // Unknown extension fields are not interpreted as standard rules.
        let parsed = strict_push_rules(Some(&raw(json!({
            "global": {
                "override": [{
                    "actions": [], "default": false, "enabled": true, "rule_id": "quiet",
                    "com.example.extension": []
                }],
                "room": [], "com.example.extension": []
            },
            "com.example.extension": []
        }))))
        .unwrap();
        let rule = parsed.override_.iter().next().unwrap();
        assert!(rule.conditions.is_empty());
        assert!(rule.actions.is_empty());
    }

    #[tokio::test]
    async fn valid_empty_push_policy_denies_instead_of_using_server_defaults() {
        for content in [
            json!({"global": {}}),
            json!({"global": {"content": [], "override": [], "room": [], "sender": [], "underride": []}}),
        ] {
            let rules = strict_push_rules(Some(&raw(content))).unwrap();
            assert!(rules.iter().next().is_none());
            assert_eq!(
                evaluate_policy(
                    context(100),
                    rules,
                    user_id!("@caller:example.org"),
                    &event(json!({"user_ids": ["@recipient:example.org"], "room": true})),
                )
                .await,
                Err(PolicyFailure::NotificationDenied)
            );
        }
    }

    #[test]
    fn policy_uses_only_the_bound_original_notification() {
        let room = room_id!("!room:example.org");
        let caller = user_id!("@caller:example.org");
        assert_eq!(
            require_event_binding(&event(json!({})), room, caller),
            Ok(())
        );
        for changed in [
            json!({"type": "m.rtc.notification", "sender": "@other:example.org"}),
            json!({"type": "m.rtc.notification", "sender": "@caller:example.org", "room_id": "!other:example.org"}),
            json!({"type": "m.room.encrypted", "sender": "@caller:example.org"}),
            json!({"type": "m.rtc.notification"}),
        ] {
            assert_eq!(
                require_event_binding(&raw::<Value>(changed), room, caller),
                Err(PolicyFailure::InvalidEvent)
            );
        }
    }

    #[tokio::test]
    async fn exact_user_mention_allows_banner_without_room_mention_permission() {
        let rules = Ruleset::server_default(user_id!("@recipient:example.org"));
        let result = evaluate_policy(
            context(0),
            rules,
            user_id!("@caller:example.org"),
            &event(json!({"user_ids": ["@recipient:example.org"]})),
        )
        .await;
        assert_eq!(result, Ok(RoomMentionPermission::Denied));
    }

    #[tokio::test]
    async fn room_mention_requires_actual_power_and_notify_action() {
        let caller = user_id!("@caller:example.org");
        let notification = event(json!({"room": true}));
        let defaults = || Ruleset::server_default(user_id!("@recipient:example.org"));
        assert_eq!(
            evaluate_policy(context(0), defaults(), caller, &notification).await,
            Err(PolicyFailure::NotificationDenied)
        );
        assert_eq!(
            evaluate_policy(context(50), defaults(), caller, &notification).await,
            Ok(RoomMentionPermission::Allowed)
        );
        assert_eq!(
            evaluate_policy(context(50), Ruleset::new(), caller, &notification).await,
            Err(PolicyFailure::NotificationDenied)
        );
    }

    #[tokio::test]
    async fn muted_rules_override_even_explicit_user_mentions() {
        let mut content = serde_json::to_value(PushRulesEventContent::new(
            Ruleset::server_default(user_id!("@recipient:example.org")),
        ))
        .unwrap();
        content["global"]["override"].as_array_mut().unwrap().insert(0, json!({
            "rule_id": "!room:example.org", "default": false, "enabled": true,
            "conditions": [{"kind": "event_match", "key": "room_id", "pattern": "!room:example.org"}],
            "actions": []
        }));
        let rules = strict_push_rules(Some(&raw(content))).unwrap();
        assert_eq!(
            evaluate_policy(
                context(100),
                rules,
                user_id!("@caller:example.org"),
                &event(json!({"user_ids": ["@recipient:example.org"]}))
            )
            .await,
            Err(PolicyFailure::NotificationDenied)
        );
    }

    #[tokio::test]
    async fn missing_power_context_and_untargeted_notifications_fail_closed() {
        let mut no_power = context(100);
        no_power.power_levels = None;
        let defaults = || Ruleset::server_default(user_id!("@recipient:example.org"));
        let caller = user_id!("@caller:example.org");
        assert_eq!(
            evaluate_policy(
                no_power,
                defaults(),
                caller,
                &event(json!({"user_ids": ["@recipient:example.org"]}))
            )
            .await,
            Err(PolicyFailure::PolicyUnavailable)
        );
        assert_eq!(
            evaluate_policy(
                context(100),
                defaults(),
                caller,
                &event(json!({"user_ids": ["@someone:example.org"]}))
            )
            .await,
            Err(PolicyFailure::NotificationDenied)
        );
    }
}
