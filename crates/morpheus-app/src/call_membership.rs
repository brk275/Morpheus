//! Shared content checks for authenticated call membership references.
//!
//! Callers supply the Matrix SDK's actual sender/device verification result.
//! Matching this content does not establish current participation, slot state,
//! invitation eligibility, or consent to join a call.

use morpheus_rtc::InteropMode;
use serde_json::Value;

pub(crate) struct MembershipEvidence<'a> {
    pub(crate) sender: &'a str,
    pub(crate) device: Option<&'a str>,
    pub(crate) verified: bool,
    pub(crate) sticky_key: &'a str,
    pub(crate) content: &'a Value,
}

/// The independently resolved identity and logical join to compare against.
pub(crate) struct MembershipIdentity<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) device_id: &'a str,
    pub(crate) membership_id: &'a str,
}

pub(crate) fn matches_membership(
    evidence: MembershipEvidence<'_>,
    expected: MembershipIdentity<'_>,
    wire_slot: &str,
    mode: InteropMode,
) -> bool {
    let content = evidence.content;
    if !evidence.verified
        || evidence.sender != expected.user_id
        || evidence.device != Some(expected.device_id)
        || evidence.sticky_key != expected.membership_id
        || content.get("slot_id").and_then(Value::as_str) != Some(wire_slot)
        || content.pointer("/application/type").and_then(Value::as_str) != Some("m.call")
        || content.pointer("/member/id").and_then(Value::as_str) != Some(expected.membership_id)
    {
        return false;
    }
    let key = content.get("sticky_key");
    let unstable = content.get("msc4354_sticky_key");
    if key.is_none() && unstable.is_none() {
        return false;
    }
    if [key, unstable]
        .into_iter()
        .flatten()
        .any(|value| value.as_str() != Some(expected.membership_id))
    {
        return false;
    }
    let membership = content.pointer("/member/membership");
    match mode {
        InteropMode::Current if membership.and_then(Value::as_str) == Some("join") => {}
        InteropMode::Sticky2025
            if membership.is_none() || membership.and_then(Value::as_str) == Some("join") => {}
        _ => return false,
    }
    // Current content does not require these redundant claims. If present,
    // neither dialect may contradict the verified sending device/user.
    [
        ("/member/user_id", expected.user_id),
        ("/member/device_id", expected.device_id),
    ]
    .into_iter()
    .all(|(path, expected)| {
        content
            .pointer(path)
            .is_none_or(|value| value.as_str() == Some(expected))
    })
}
