// Copyright 2026 Element Creations Ltd.
//
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial
// Please see LICENSE in the repository root for full details.

//! Which generation of MatrixRTC a room is read in: the feeder's only
//! format-specific decisions, behind [`IngestDialect`]. The core implements it
//! for each [`MembershipFormat`](crate::compat::MembershipFormat); an
//! application feeding through [`RoomFeeder`](super::RoomFeeder) may bring its
//! own.

use std::collections::BTreeMap;

use crate::{
    EventIn, MaybeSend, RawSlotEvent, RawStickyEvent, ReceivedEncryptionKey, ToDeviceMessageIn,
};

/// How one room's membership and media keys are read.
pub trait IngestDialect: MaybeSend {
    /// The room state event types membership is also read from.
    fn membership_state_event_types(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether the room's slots are read; membership then waits for them, so a
    /// member is never briefly joined to a slot the room's state says is closed.
    fn reads_slots(&self) -> bool {
        true
    }

    /// Project one original wire state key into this room's internal slot id.
    /// Returning `None` excludes another generation's distinct slot authority.
    fn slot_id(&self, wire_slot_id: &str) -> Option<String> {
        Some(wire_slot_id.to_owned())
    }

    /// The room's complete slot snapshot. Stable state wins over unstable state
    /// for the SAME original state key before any dialect projection. Malformed
    /// content remains a closed slot rather than exposing the other spelling.
    fn current_slots(
        &self,
        room_id: &str,
        stable: Vec<EventIn>,
        unstable: Vec<EventIn>,
    ) -> Vec<RawSlotEvent> {
        let mut current = BTreeMap::new();
        for event in unstable.into_iter().chain(stable) {
            if let Some(state_key) = event.state_key.clone() {
                current.insert(state_key, event);
            }
        }
        current
            .into_iter()
            .filter_map(|(wire_slot_id, event)| {
                Some(RawSlotEvent {
                    room_id: room_id.to_owned(),
                    slot_id: self.slot_id(&wire_slot_id)?,
                    content: serde_json::from_value(event.content).unwrap_or_default(),
                })
            })
            .collect()
    }

    /// The room's complete current membership, from its sticky member events
    /// and the latest membership state of [`Self::membership_state_event_types`].
    fn current_membership(
        &self,
        room_id: &str,
        sticky: Vec<EventIn>,
        state: Vec<EventIn>,
    ) -> Vec<RawStickyEvent>;

    /// A media key from a to-device message of a type beyond the spec's, which
    /// the client subscribed to.
    fn parse_key(&self, _message: &ToDeviceMessageIn) -> Option<ReceivedEncryptionKey> {
        None
    }
}
