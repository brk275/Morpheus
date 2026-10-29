// Copyright 2026 Element Creations Ltd.
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial

//! One immutable auth contract and terminal admission latch for one call.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use matrix_rtc_core::{JoinedMembership, RtcTransport};
use matrix_rtc_transport::{OwnMemberClaims, livekit::LiveKitAuthDialect};
use tokio::sync::watch;

use crate::engine::{CallAdmission, EngineHandle};

// No eviction: losing an old binding could make a retained key alias unsafe.
// Exceeding these call-scoped bounds is an explicit terminal refusal.
const MAX_IDENTITY_BINDINGS: usize = 4_096;
const MAX_IDENTITY_FIELD_BYTES: usize = 1_024;
const MAX_IDENTITY_BYTES: usize = 4 * 1_024 * 1_024;

#[derive(Clone, PartialEq, Eq)]
struct Owner {
    user: String,
    device: String,
    member: String,
}

#[derive(Default)]
struct Bindings {
    members: BTreeMap<String, Owner>,
    identities: BTreeMap<String, Owner>,
}

pub(super) struct AuthGuard {
    dialect: LiveKitAuthDialect,
    own: Owner,
    memberships: watch::Receiver<Vec<JoinedMembership>>,
    pub(super) active: Arc<AtomicBool>,
    history: Mutex<Bindings>,
    engine: Mutex<Option<EngineHandle>>,
}

impl Owner {
    fn valid(&self) -> bool {
        [&self.user, &self.device, &self.member]
            .iter()
            .all(|field| !field.is_empty() && field.len() <= MAX_IDENTITY_FIELD_BYTES)
    }

    fn bytes(&self) -> usize {
        self.user.len() + self.device.len() + self.member.len()
    }
}

impl Bindings {
    fn bytes(&self) -> usize {
        self.members
            .iter()
            .chain(self.identities.iter())
            .map(|(key, owner)| key.len() + owner.bytes())
            .sum()
    }

    fn extend_checked(&mut self, next: Bindings) -> bool {
        let members = next
            .members
            .iter()
            .filter(|(key, _)| !self.members.contains_key(*key));
        let identities = next
            .identities
            .iter()
            .filter(|(key, _)| !self.identities.contains_key(*key));
        let new_members = members.clone().count();
        let new_identities = identities.clone().count();
        let added_bytes: usize = members
            .chain(identities)
            .map(|(key, owner)| key.len() + owner.bytes())
            .sum();
        let valid = self.members.len() + new_members <= MAX_IDENTITY_BINDINGS
            && self.identities.len() + new_identities <= MAX_IDENTITY_BINDINGS
            && self.bytes() + added_bytes <= MAX_IDENTITY_BYTES
            && next
                .members
                .iter()
                .all(|(id, owner)| self.members.get(id).is_none_or(|held| held == owner))
            && next
                .identities
                .iter()
                .all(|(id, owner)| self.identities.get(id).is_none_or(|held| held == owner));
        if valid {
            self.members.extend(next.members);
            self.identities.extend(next.identities);
        }
        valid
    }
}

impl AuthGuard {
    pub(super) fn new(
        dialect: LiveKitAuthDialect,
        own: OwnMemberClaims,
        memberships: watch::Receiver<Vec<JoinedMembership>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            dialect,
            own: Owner {
                user: own.user_id,
                device: own.device_id,
                member: own.member_id,
            },
            memberships,
            active: Arc::new(AtomicBool::new(true)),
            history: Mutex::new(Bindings::default()),
            engine: Mutex::new(None),
        })
    }

    fn fail(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            let engine = self
                .engine
                .lock()
                .expect("auth guard mutex poisoned")
                .clone();
            if let Some(engine) = engine {
                engine.reject_identity_contract();
            }
        }
    }

    fn derive(&self, snapshot: &[JoinedMembership]) -> Option<Bindings> {
        if !self.own.valid() || snapshot.len() > MAX_IDENTITY_BINDINGS {
            return None;
        }
        // Bound raw fields before cloning or hashing any peer-controlled data.
        let mut snapshot_bytes = self.own.bytes();
        for member in snapshot {
            let device = member.origin.sender_device_id()?;
            for field in [member.sender.as_str(), device, member.member_id.as_str()] {
                if field.is_empty() || field.len() > MAX_IDENTITY_FIELD_BYTES {
                    return None;
                }
                snapshot_bytes += field.len();
                if snapshot_bytes > MAX_IDENTITY_BYTES {
                    return None;
                }
            }
        }
        let mut bindings = Bindings::default();
        let own_identity =
            self.dialect
                .identity(&self.own.user, &self.own.device, &self.own.member)?;
        bindings
            .members
            .insert(self.own.member.clone(), self.own.clone());
        bindings.identities.insert(own_identity, self.own.clone());
        for member in snapshot {
            if member.transports.iter().any(|transport| {
                matches!(transport,
                RtcTransport::Unsupported(raw) if raw.transport_type == "livekit")
            }) {
                return None;
            }
            let owner = if member.member_id == self.own.member {
                if member.sender != self.own.user
                    || member.origin.sender_device_id() != Some(self.own.device.as_str())
                {
                    return None;
                }
                self.own.clone()
            } else {
                Owner {
                    user: member.sender.clone(),
                    device: member.origin.sender_device_id()?.to_owned(),
                    member: member.member_id.clone(),
                }
            };
            let identity = self
                .dialect
                .identity(&owner.user, &owner.device, &owner.member)?;
            if bindings
                .members
                .get(&owner.member)
                .is_some_and(|held| held != &owner)
                || bindings
                    .identities
                    .get(&identity)
                    .is_some_and(|held| held != &owner)
            {
                return None;
            }
            bindings.members.insert(owner.member.clone(), owner.clone());
            bindings.identities.insert(identity, owner);
        }
        Some(bindings)
    }

    pub(super) fn admit(&self, snapshot: &[JoinedMembership]) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        let Some(bindings) = self.derive(snapshot) else {
            self.fail();
            return false;
        };
        let accepted = self
            .history
            .lock()
            .expect("auth history mutex poisoned")
            .extend_checked(bindings);
        if !accepted {
            self.fail();
        }
        accepted && self.active.load(Ordering::Acquire)
    }

    pub(super) fn map(
        &self,
        user: &str,
        device: &str,
        member: &str,
        authoritative: Option<&[JoinedMembership]>,
    ) -> Option<String> {
        let current;
        let snapshot = match authoritative {
            Some(snapshot) => snapshot,
            None => {
                current = self.memberships.borrow();
                &current
            }
        };
        if !self.admit(snapshot) {
            return None;
        }
        // A verified tuple may outlive the exact snapshot used to verify it.
        // Pin it even when temporarily absent, so a later same-id different-owner
        // membership cannot replay its held key under the replacement's alias.
        if [user, device, member]
            .iter()
            .any(|field| field.is_empty() || field.len() > MAX_IDENTITY_FIELD_BYTES)
        {
            self.fail();
            return None;
        }
        let target = Owner {
            user: user.into(),
            device: device.into(),
            member: member.into(),
        };
        let identity = self.dialect.identity(user, device, member)?;
        let mut binding = Bindings::default();
        binding.members.insert(member.into(), target.clone());
        binding.identities.insert(identity.clone(), target);
        let accepted = self
            .history
            .lock()
            .expect("auth history mutex poisoned")
            .extend_checked(binding);
        if !accepted {
            self.fail();
            return None;
        }
        // Truly absent membership stays pending; present/historical conflicting
        // ownership above is terminal rather than a benign pending condition.
        let own = user == self.own.user && device == self.own.device && member == self.own.member;
        let present = snapshot.iter().any(|entry| {
            entry.sender == user
                && entry.member_id == member
                && entry.origin.sender_device_id() == Some(device)
        });
        if (!own && !present) || !self.active.load(Ordering::Acquire) {
            return None;
        }
        Some(identity)
    }

    pub(super) fn admission(self: &Arc<Self>) -> CallAdmission {
        let check = Arc::clone(self);
        let bind = Arc::clone(self);
        CallAdmission {
            active: Arc::clone(&self.active),
            check: Arc::new(move |snapshot| check.admit(snapshot)),
            bind: Arc::new(move |engine| {
                *bind.engine.lock().expect("auth guard mutex poisoned") = Some(engine.clone());
                if !bind.active.load(Ordering::Acquire) {
                    engine.reject_identity_contract();
                }
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_rtc_core::EventOrigin;

    fn member(user: &str, device: Option<&str>, id: &str) -> JoinedMembership {
        JoinedMembership {
            room_id: "!room:example.org".into(),
            slot_id: "m.call#ROOM".into(),
            sender: user.into(),
            origin: EventOrigin::encrypted(device.map(str::to_owned)),
            sticky_key: id.into(),
            member_id: id.into(),
            membership_event_id: None,
            membership_ts: None,
            origin_server_ts: None,
            application: "m.call".into(),
            transports: Vec::new(),
            can_subscribe: Vec::new(),
        }
    }

    fn guard() -> Arc<AuthGuard> {
        let (_tx, rx) = watch::channel(Vec::new());
        AuthGuard::new(
            LiveKitAuthDialect::LegacyService,
            OwnMemberClaims {
                user_id: "@alice:example.org".into(),
                device_id: "ALICE".into(),
                member_id: "alice-member".into(),
            },
            rx,
        )
    }

    #[test]
    fn pending_absence_can_resolve_without_rejecting_the_call() {
        let guard = guard();
        assert_eq!(guard.map("@bob:example.org", "BOB", "bob", Some(&[])), None);
        assert!(guard.active.load(Ordering::Acquire));
        let bob = member("@bob:example.org", Some("BOB"), "bob");
        assert_eq!(
            guard.map("@bob:example.org", "BOB", "bob", Some(&[bob])),
            Some("@bob:example.org:BOB".into())
        );
    }

    #[test]
    fn previously_verified_absent_tuple_cannot_replay_under_a_new_owner() {
        let guard = guard();
        assert_eq!(
            guard.map("@bob:example.org", "BOB", "shared", Some(&[])),
            None
        );
        assert!(!guard.admit(&[member("@mallory:example.org", Some("MALLORY"), "shared")]));
        assert!(!guard.active.load(Ordering::Acquire));
        let present = self::guard();
        let replacement = member("@mallory:example.org", Some("MALLORY"), "shared");
        assert_eq!(
            present.map("@bob:example.org", "BOB", "shared", Some(&[replacement])),
            None
        );
        assert!(!present.active.load(Ordering::Acquire));
    }

    #[test]
    fn history_and_snapshot_bounds_refuse_without_eviction() {
        let guard = guard();
        let too_long = "x".repeat(MAX_IDENTITY_FIELD_BYTES + 1);
        assert!(!guard.admit(&[member("@bob:example.org", Some("BOB"), &too_long)]));
        let snapshot = self::guard();
        let bob = member("@bob:example.org", Some("BOB"), "bob");
        assert!(!snapshot.admit(&vec![bob; MAX_IDENTITY_BINDINGS + 1]));
        let mut bindings = Bindings::default();
        let owner = Owner {
            user: "u".into(),
            device: "d".into(),
            member: "m".into(),
        };
        for index in 0..MAX_IDENTITY_BINDINGS {
            bindings.members.insert(index.to_string(), owner.clone());
        }
        let mut next = Bindings::default();
        next.members.insert("another".into(), owner.clone());
        assert!(!bindings.extend_checked(next));
        assert_eq!(bindings.members.len(), MAX_IDENTITY_BINDINGS);
        let mut bytes = Bindings::default();
        for index in 0..1_000 {
            let large = Owner {
                user: "u".repeat(1_024),
                device: "d".repeat(1_024),
                member: index.to_string(),
            };
            bytes.members.insert(index.to_string(), large.clone());
            bytes.identities.insert(format!("alias{index}"), large);
        }
        assert!(bytes.bytes() < MAX_IDENTITY_BYTES);
        let mut next = Bindings::default();
        for index in 1_000..1_030 {
            let large = Owner {
                user: "u".repeat(1_024),
                device: "d".repeat(1_024),
                member: index.to_string(),
            };
            next.members.insert(index.to_string(), large.clone());
            next.identities.insert(format!("alias{index}"), large);
        }
        assert!(!bytes.extend_checked(next));
        assert_eq!(bytes.members.len(), 1_000);
    }

    #[test]
    fn own_alias_is_reserved_before_echo_and_refusal_is_latched() {
        let guard = guard();
        let collision = member("@alice:example.org", Some("ALICE"), "other-member");
        assert!(!guard.admit(&[collision]));
        assert!(!guard.admit(&[]));
        assert_eq!(
            guard.map("@alice:example.org", "ALICE", "alice-member", Some(&[])),
            None
        );
    }

    #[test]
    fn duplicate_aliases_on_different_foci_are_still_ambiguous() {
        let guard = guard();
        let mut bob = member("@bob:example.org", Some("BOB"), "bob");
        let mut duplicate = member("@bob:example.org", Some("BOB"), "other-bob");
        bob.transports = vec![RtcTransport::LiveKit(matrix_rtc_core::LiveKitTransport {
            livekit_service_url: "https://first.example.org".into(),
        })];
        duplicate.transports = vec![RtcTransport::LiveKit(matrix_rtc_core::LiveKitTransport {
            livekit_service_url: "https://second.example.org".into(),
        })];
        assert!(!guard.admit(&[bob, duplicate]));
    }

    #[test]
    fn remote_alias_collision_and_reassignment_are_terminal() {
        let guard = guard();
        let bob = member("@bob:example.org", Some("BOB"), "bob");
        assert!(guard.admit(std::slice::from_ref(&bob)));
        assert!(guard.admit(&[]));
        let replacement = member("@bob:example.org", Some("BOB"), "replacement");
        assert!(!guard.admit(&[replacement]));
        assert!(!guard.admit(&[bob]));
    }

    #[test]
    fn same_member_identity_change_and_missing_device_are_terminal() {
        let guard = guard();
        let bob = member("@bob:example.org", Some("BOB"), "bob");
        assert!(guard.admit(&[bob]));
        assert!(!guard.admit(&[member("@mallory:example.org", Some("BOB"), "bob")]));
        let missing = self::guard();
        assert!(!missing.admit(&[member("@bob:example.org", None, "bob")]));
    }

    #[test]
    fn same_owner_membership_refresh_remains_valid() {
        let guard = guard();
        let mut bob = member("@bob:example.org", Some("BOB"), "bob");
        bob.membership_ts = Some(1);
        assert!(guard.admit(std::slice::from_ref(&bob)));
        bob.membership_ts = Some(2);
        assert!(guard.admit(&[bob]));
    }
}
