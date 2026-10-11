//! The pinned RTC dialect boundary for an app-owned, receipt-bearing send.
//! This does not send, invent a sticky key, or enable legacy ringing.

use std::fmt;

use matrix_rtc_core::compat::{MembershipFormat, ingest::outbound_dialect};
use serde_json::Value;

use crate::InteropMode;

pub struct NotificationDialect {
    inner: matrix_rtc_core::compat::OutboundDialect,
    membership_slot: String,
}

impl fmt::Debug for NotificationDialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NotificationDialect { .. }")
    }
}

impl NotificationDialect {
    pub fn new(
        mode: InteropMode,
        user_id: &str,
        device_id: &str,
        room_id: &str,
        slot_id: &str,
    ) -> Option<Self> {
        // Morpheus currently offers only the encrypted room-wide call slot.
        if mode == InteropMode::RoomState || slot_id != "m.call#room" {
            return None;
        }
        let format = MembershipFormat::from(mode);
        Some(Self {
            inner: outbound_dialect(format, user_id, device_id, room_id, slot_id),
            membership_slot: format.token_slot_id(slot_id).into_owned(),
        })
    }

    pub fn membership_slot(&self) -> &str {
        &self.membership_slot
    }

    pub fn event_type(&self) -> &'static str {
        matrix_rtc_core::wire_event_type("m.rtc.notification")
    }

    pub fn render(&self, canonical: Value) -> Value {
        self.inner
            .rewrite_notification("m.rtc.notification", canonical)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn current_and_matrix2_use_the_pinned_wire_adapter() {
        let canonical = json!({
            "application": {"type": "m.call", "notification_type": "ring",
                "device_id": "DEVICE", "sender_ts": 1000, "lifetime": 30000,
                "m.call.intent": "audio"},
            "notification_type": "ring", "sender_ts": 1000, "lifetime": 30000,
            "m.call.intent": "audio", "m.text": [{"body": "Call invitation"}],
            "m.mentions": {"user_ids": ["@peer:example.org"], "room": false},
            "m.relates_to": {"rel_type": "m.reference", "event_id": "$membership"}
        });
        for (mode, slot) in [
            (InteropMode::Current, "m.call#room"),
            (InteropMode::Sticky2025, "m.call#ROOM"),
        ] {
            let dialect = NotificationDialect::new(
                mode,
                "@self:example.org",
                "DEVICE",
                "!room:example.org",
                "m.call#room",
            )
            .unwrap();
            let wire = dialect.render(canonical.clone());
            assert_eq!(dialect.membership_slot(), slot);
            assert_eq!(dialect.event_type(), "org.matrix.msc4075.rtc.notification");
            assert_eq!(wire["m.relates_to"], canonical["m.relates_to"]);
            assert_eq!(wire["m.mentions"], canonical["m.mentions"]);
            assert_eq!(wire["lifetime"], 30000);
            assert!(wire.get("sticky_key").is_none());
            assert!(wire.get("msc4354_sticky_key").is_none());
            if mode == InteropMode::Current {
                assert_eq!(wire, canonical);
            } else {
                assert!(wire.get("application").is_none());
                assert!(wire.get("device_id").is_none());
            }
        }
    }

    #[test]
    fn legacy_and_arbitrary_slots_cannot_request_ringing() {
        for (mode, slot) in [
            (InteropMode::RoomState, "m.call#room"),
            (InteropMode::Current, "m.call#other"),
            (InteropMode::Sticky2025, "m.call#ROOM"),
        ] {
            assert!(
                NotificationDialect::new(
                    mode,
                    "@self:example.org",
                    "DEVICE",
                    "!room:example.org",
                    slot
                )
                .is_none()
            );
        }
    }
}
