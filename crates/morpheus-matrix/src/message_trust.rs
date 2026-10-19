//! Per-message authenticity warnings from the SDK's shield state in its lax
//! mode: nothing is flagged only because a person is unverified, just when
//! the sender, their device or the encryption is wrong or cannot be checked.
//! The SDK re-evaluates shields as device and identity data change.
//! In-room verification requests also get a fixed line instead of the text
//! their sender wrote.

use matrix_sdk::ruma::events::room::message::MessageType;
use matrix_sdk_ui::timeline::{
    EventTimelineItem, TimelineEventShieldState as Shield, TimelineEventShieldStateCode as Code,
    TimelineItemContent,
};

/// A fixed warning for one message, and whether it is serious (red) rather
/// than informational (grey).
pub(crate) fn warning(item: &EventTimelineItem) -> Option<(&'static str, bool)> {
    shield_warning(item.get_shield(false))
}

fn shield_warning(shield: Shield) -> Option<(&'static str, bool)> {
    match shield {
        Shield::Red { code } => Some((code_text(code), true)),
        Shield::Grey { code } => Some((code_text(code), false)),
        Shield::None => None,
    }
}

fn code_text(code: Code) -> &'static str {
    match code {
        Code::AuthenticityNotGuaranteed => "Its authenticity can't be confirmed on this device.",
        Code::UnknownDevice => "Sent from an unknown or deleted device.",
        Code::UnsignedDevice => "Sent from a device its owner hasn't verified.",
        Code::UnverifiedIdentity => "Sent by someone you haven't verified.",
        Code::VerificationViolation => "Sent after this person's verified identity changed.",
        Code::MismatchedSender => "The sender doesn't match the device that encrypted it.",
        Code::SentInClear => "Sent without encryption in an encrypted room.",
    }
}

/// In-room verification requests carry fallback text the sender wrote for
/// clients without in-room verification. The timeline shows a fixed line
/// instead; the request banner offers the actual choices.
pub(crate) fn verification_request_line(
    content: &TimelineItemContent,
    own: bool,
) -> Option<&'static str> {
    request_line(content.as_message().map(|message| message.msgtype()), own)
}

fn request_line(msgtype: Option<&MessageType>, own: bool) -> Option<&'static str> {
    match msgtype {
        Some(MessageType::VerificationRequest(_)) if own => {
            Some("You sent a verification request.")
        }
        Some(MessageType::VerificationRequest(_)) => Some("Sent a verification request."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const CODES: [Code; 7] = [
        Code::AuthenticityNotGuaranteed,
        Code::UnknownDevice,
        Code::UnsignedDevice,
        Code::UnverifiedIdentity,
        Code::VerificationViolation,
        Code::MismatchedSender,
        Code::SentInClear,
    ];

    #[test]
    fn red_shields_are_serious_grey_are_notes_and_none_shows_nothing() {
        let code = Code::SentInClear;
        let text = code_text(code);
        assert_eq!(shield_warning(Shield::Red { code }), Some((text, true)));
        assert_eq!(shield_warning(Shield::Grey { code }), Some((text, false)));
        assert_eq!(shield_warning(Shield::None), None);
    }

    #[test]
    fn every_shield_code_has_its_own_single_line_text() {
        let texts: BTreeSet<_> = CODES.into_iter().map(code_text).collect();
        assert_eq!(texts.len(), CODES.len());
        for text in texts {
            assert!(!text.contains('\n') && text.len() <= 80, "{text}");
        }
    }

    #[test]
    fn verification_requests_show_a_fixed_line_instead_of_sender_text() {
        use matrix_sdk::ruma::UserId;
        use matrix_sdk::ruma::events::key::verification::VerificationMethod;
        use matrix_sdk::ruma::events::room::message::KeyVerificationRequestEventContent;

        let content = KeyVerificationRequestEventContent::new(
            "Your client cannot verify here.".into(),
            vec![VerificationMethod::SasV1],
            "DEVICE".into(),
            UserId::parse("@member:example.org").unwrap(),
        );
        let request = MessageType::VerificationRequest(content);
        let own = request_line(Some(&request), true);
        let other = request_line(Some(&request), false);
        assert!(own.is_some() && other.is_some() && own != other);
        assert!(!other.unwrap().contains("client"));
        let text = MessageType::text_plain("hello");
        assert_eq!(request_line(Some(&text), false), None);
        assert_eq!(request_line(None, true), None);
    }
}
