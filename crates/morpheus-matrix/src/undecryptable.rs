//! What the timeline shows for a message this device could not decrypt.
//! The SDK retries as keys arrive (from key storage, another verified
//! device or the sender) and then replaces the notice with the message.
//! Each notice names only the cause the SDK reported and the one thing the
//! person can do about it, if anything.

use matrix_sdk_crypto::types::events::UtdCause;
use matrix_sdk_ui::timeline::TimelineItemContent;

/// The notice for undecryptable content, or `None` for anything else.
pub(crate) fn notice(content: &TimelineItemContent) -> Option<&'static str> {
    content
        .as_unable_to_decrypt()
        .map(|message| cause_notice(message.utd_cause()))
}

fn cause_notice(cause: Option<UtdCause>) -> &'static str {
    match cause {
        Some(UtdCause::SentBeforeWeJoined) => {
            "Sent before you joined this room, so its keys were never shared with you."
        }
        Some(UtdCause::HistoricalMessageAndDeviceIsUnverified) => {
            "Verify this device in Security settings to read messages from before it signed in."
        }
        Some(UtdCause::HistoricalMessageAndBackupIsDisabled) => {
            "Messages from before this device signed in aren't available: key storage is off."
        }
        Some(UtdCause::WithheldForUnverifiedOrInsecureDevice) => {
            "Keys are shared only with verified devices. Verify this device in Security settings."
        }
        Some(UtdCause::WithheldBySender) => {
            "The sender didn't share the keys for this message with this device."
        }
        Some(UtdCause::VerificationViolation) => {
            "Hidden because the sender's identity changed after you verified it."
        }
        Some(UtdCause::UnsignedDevice) => {
            "Hidden because the sender hasn't verified the device that sent it."
        }
        Some(UtdCause::UnknownDevice) => {
            "Hidden because the device that sent it couldn't be confirmed."
        }
        Some(UtdCause::Unknown) | None => {
            "Waiting for keys to read this. If it stays hidden, recover keys in Security settings."
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const NAMED: [UtdCause; 8] = [
        UtdCause::SentBeforeWeJoined,
        UtdCause::VerificationViolation,
        UtdCause::UnsignedDevice,
        UtdCause::UnknownDevice,
        UtdCause::HistoricalMessageAndBackupIsDisabled,
        UtdCause::WithheldForUnverifiedOrInsecureDevice,
        UtdCause::WithheldBySender,
        UtdCause::HistoricalMessageAndDeviceIsUnverified,
    ];

    #[test]
    fn each_reported_cause_gets_its_own_single_line_notice() {
        let waiting = cause_notice(None);
        assert_eq!(cause_notice(Some(UtdCause::Unknown)), waiting);
        let notices: BTreeSet<_> = NAMED.into_iter().map(Some).map(cause_notice).collect();
        assert_eq!(notices.len(), NAMED.len());
        assert!(!notices.contains(waiting));
        for notice in notices.into_iter().chain([waiting]) {
            assert!(!notice.contains('\n') && notice.len() <= 90, "{notice}");
        }
    }

    #[test]
    fn notices_offer_only_an_action_that_can_help() {
        for cause in [
            UtdCause::HistoricalMessageAndDeviceIsUnverified,
            UtdCause::WithheldForUnverifiedOrInsecureDevice,
        ] {
            assert!(
                cause_notice(Some(cause)).contains("Verify this device"),
                "{cause:?}"
            );
        }
        assert!(cause_notice(None).contains("Security settings"));
        // Nothing on this device can recover these.
        for cause in [UtdCause::SentBeforeWeJoined, UtdCause::WithheldBySender] {
            assert!(
                !cause_notice(Some(cause)).contains("Security settings"),
                "{cause:?}"
            );
        }
    }
}
