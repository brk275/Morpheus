// Copyright 2026 Morpheus contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! One immutable auth contract and terminal admission latch for one call.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use matrix_rtc_core::{JoinedMembership, RtcTransport};
use matrix_rtc_transport::{OwnMemberClaims, livekit::LiveKitAuthDialect};
use matrix_rtc_transport::keys::KeyGeneration;
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

struct GenerationBinding {
    member: String,
    lease: KeyGeneration,
}

#[derive(Default)]
struct Bindings {
    members: BTreeMap<String, Owner>,
    identities: BTreeMap<String, Owner>,
    current: BTreeMap<String, GenerationBinding>,
}

pub(super) struct AuthGuard {
    dialect: LiveKitAuthDialect,
    own: Owner,
    memberships: watch::Receiver<Vec<JoinedMembership>>,
    pub(super) active: Arc<AtomicBool>,
    history: Mutex<Bindings>,
    seeded: AtomicBool,
    engine: Mutex<Option<EngineHandle>>,
}

impl Owner {
    fn valid(&self) -> bool {
        [&self.user, &self.device, &self.member]
            .iter()
            .all(|field| !field.is_empty() && field.len() <= MAX_IDENTITY_FIELD_BYTES)
    }

    fn same_principal(&self, other: &Self) -> bool {
        self.user == other.user && self.device == other.device
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
            .sum::<usize>()
            + self
                .current
                .iter()
                .map(|(id, value)| id.len() + value.member.len())
                .sum::<usize>()
    }

    #[cfg(test)]
    fn extend_checked(&mut self, next: Bindings) -> bool {
        self.extend_owners(&next, false)
    }

    fn extend_owners(&mut self, next: &Bindings, legacy: bool) -> bool {
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
            && next.identities.iter().all(|(id, owner)| {
                self.identities
                    .get(id)
                    .is_none_or(|held| held == owner || (legacy && held.same_principal(owner)))
            });
        if valid {
            for (id, owner) in &next.members {
                self.members
                    .entry(id.clone())
                    .or_insert_with(|| owner.clone());
            }
            for (id, owner) in &next.identities {
                self.identities
                    .entry(id.clone())
                    .or_insert_with(|| owner.clone());
            }
        }
        valid
    }

    fn reconcile(&mut self, next: &BTreeMap<String, Owner>) -> bool {
        // Only the borrowed core callback (or once-only initial seed) reaches
        // this transition. Membership ids can legitimately return, but every
        // old lease stays revoked; a returning id receives a fresh lease.
        for (identity, held) in &self.current {
            if !next.contains_key(identity) {
                held.lease.active.store(false, Ordering::Release);
            }
        }
        for (identity, owner) in next {
            let held = self.current.get(identity);
            if held.is_some_and(|held| {
                held.member == owner.member && held.lease.active.load(Ordering::Acquire)
            }) {
                continue;
            }
            let extra = match held {
                Some(held) => self.bytes() - held.member.len() + owner.member.len(),
                None => self.bytes() + identity.len() + owner.member.len(),
            };
            if extra > MAX_IDENTITY_BYTES {
                return false;
            }
            let serial = match self.current.get(identity) {
                Some(held) => {
                    held.lease.active.store(false, Ordering::Release);
                    Arc::clone(&held.lease.serial)
                }
                None => Arc::new(tokio::sync::Mutex::new(())),
            };
            self.current.insert(
                identity.clone(),
                GenerationBinding {
                    member: owner.member.clone(),
                    lease: KeyGeneration {
                        active: Arc::new(AtomicBool::new(true)),
                        serial,
                    },
                },
            );
        }
        true
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
            seeded: AtomicBool::new(false),
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

    pub(super) fn include_member(&self, member: &JoinedMembership) -> bool {
        member.sender != self.own.user
            || member.origin.sender_device_id() != Some(self.own.device.as_str())
            || member.member_id == self.own.member
    }

    fn derive(&self, snapshot: &[JoinedMembership]) -> Option<Bindings> {
        if !self.own.valid() || snapshot.len() > MAX_IDENTITY_BINDINGS {
            return None;
        }
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
            let owner = Owner {
                user: member.sender.clone(),
                device: member.origin.sender_device_id()?.to_owned(),
                member: member.member_id.clone(),
            };
            if bindings
                .members
                .get(&owner.member)
                .is_some_and(|held| held != &owner)
            {
                return None;
            }
            bindings.members.insert(owner.member.clone(), owner.clone());
            // A stale membership of our own exact device cannot own this join's
            // media. Keep its immutable identity history, omit its roster/keys.
            if !self.include_member(member) {
                continue;
            }
            let identity = self
                .dialect
                .identity(&owner.user, &owner.device, &owner.member)?;
            if bindings
                .identities
                .get(&identity)
                .is_some_and(|held| held != &owner)
            {
                return None;
            }
            bindings.identities.insert(identity, owner);
        }
        Some(bindings)
    }

    /// The only pre-install transition. The view is borrowed and bounded; a
    /// second call cannot seed a newer or older view into an installed guard.
    pub(super) fn seed_initial(&self) -> bool {
        if self.seeded.swap(true, Ordering::AcqRel) {
            return self.active.load(Ordering::Acquire);
        }
        let initial = self.memberships.borrow();
        self.on_core_snapshot(&initial)
    }

    pub(super) fn admit(&self, snapshot: &[JoinedMembership]) -> bool {
        self.admit_view(snapshot, false)
    }

    pub(super) fn on_core_snapshot(&self, snapshot: &[JoinedMembership]) -> bool {
        self.admit_view(snapshot, true)
    }

    fn admit_view(&self, snapshot: &[JoinedMembership], authoritative: bool) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        let Some(bindings) = self.derive(snapshot) else {
            self.fail();
            return false;
        };
        let accepted = {
            let mut history = self.history.lock().expect("auth history mutex poisoned");
            history.extend_owners(&bindings, self.dialect == LiveKitAuthDialect::LegacyService)
                && (!authoritative || history.reconcile(&bindings.identities))
        };
        if !accepted {
            self.fail();
        }
        accepted && self.active.load(Ordering::Acquire)
    }

    pub(super) fn member_lease(&self, identity: &str, member: &str) -> Option<KeyGeneration> {
        if !self.active.load(Ordering::Acquire) {
            return None;
        }
        let history = self.history.lock().expect("auth history mutex poisoned");
        let held = history.current.get(identity)?;
        if held.member != member || !held.lease.active.load(Ordering::Acquire) {
            return None;
        }
        Some(held.lease.clone())
    }

    pub(super) fn map(
        &self,
        user: &str,
        device: &str,
        member: &str,
        observation: Option<&[JoinedMembership]>,
    ) -> Option<String> {
        let current;
        let snapshot = match observation {
            Some(snapshot) => snapshot,
            None => {
                current = self.memberships.borrow();
                &current
            }
        };
        // Both optional slices and watch views are observations, not temporal
        // authority. They may validate immutable ownership but never reconcile
        // epochs or resurrect/revoke another participation's lease.
        if !self.admit(snapshot) {
            return None;
        }
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
            .extend_owners(&binding, self.dialect == LiveKitAuthDialect::LegacyService);
        if !accepted {
            self.fail();
            return None;
        }
        let own = user == self.own.user && device == self.own.device && member == self.own.member;
        let present = snapshot.iter().any(|entry| {
            entry.sender == user
                && entry.member_id == member
                && entry.origin.sender_device_id() == Some(device)
                && self.include_member(entry)
        });
        if (!own && !present) || self.member_lease(&identity, member).is_none() {
            return None;
        }
        Some(identity)
    }

    pub(super) fn admission(self: &Arc<Self>) -> CallAdmission {
        let check = Arc::clone(self);
        let include = Arc::clone(self);
        let bind = Arc::clone(self);
        CallAdmission {
            active: Arc::clone(&self.active),
            check: Arc::new(move |snapshot| check.admit(snapshot)),
            include: Arc::new(move |member| include.include_member(member)),
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
        let guard = AuthGuard::new(
            LiveKitAuthDialect::LegacyService,
            OwnMemberClaims {
                user_id: "@alice:example.org".into(),
                device_id: "ALICE".into(),
                member_id: "alice-member".into(),
            },
            rx,
        );
        assert!(guard.seed_initial());
        guard
    }

    #[test]
    fn pending_absence_can_resolve_without_rejecting_the_call() {
        let guard = guard();
        assert_eq!(guard.map("@bob:example.org", "BOB", "bob", Some(&[])), None);
        assert!(guard.active.load(Ordering::Acquire));
        let bob = member("@bob:example.org", Some("BOB"), "bob");
        assert!(guard.on_core_snapshot(std::slice::from_ref(&bob)));
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
        assert!(!guard.on_core_snapshot(&[member("@mallory:example.org", Some("MALLORY"), "shared")]));
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
        assert!(!guard.on_core_snapshot(&[member("@bob:example.org", Some("BOB"), &too_long)]));
        let snapshot = self::guard();
        let bob = member("@bob:example.org", Some("BOB"), "bob");
        assert!(!snapshot.on_core_snapshot(&vec![bob; MAX_IDENTITY_BINDINGS + 1]));
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
        let collision = member("@mallory:example.org", Some("MALLORY"), "alice-member");
        assert!(!guard.on_core_snapshot(&[collision]));
        assert!(!guard.on_core_snapshot(&[]));
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
        assert!(!guard.on_core_snapshot(&[bob, duplicate]));
    }

    #[test]
    fn initial_seed_is_once_and_mapper_views_never_advance_generations() {
        let first = member("@bob:example.org", Some("BOB"), "first");
        let second = member("@bob:example.org", Some("BOB"), "second");
        let (tx, rx) = watch::channel(vec![first.clone()]);
        let guard = AuthGuard::new(
            LiveKitAuthDialect::LegacyService,
            OwnMemberClaims {
                user_id: "@alice:example.org".into(),
                device_id: "ALICE".into(),
                member_id: "alice-member".into(),
            },
            rx,
        );
        let identity = "@bob:example.org:BOB";
        assert!(guard.seed_initial());
        let old = guard.member_lease(identity, "first").unwrap();
        tx.send(vec![second.clone()]).unwrap();
        assert!(guard.seed_initial());
        assert!(guard.member_lease(identity, "second").is_none());
        assert!(old.active.load(Ordering::Acquire));
        assert_eq!(
            guard.map("@bob:example.org", "BOB", "second", Some(std::slice::from_ref(&second))),
            None
        );
        assert!(old.active.load(Ordering::Acquire));
        assert!(guard.on_core_snapshot(std::slice::from_ref(&second)));
        let current = guard.member_lease(identity, "second").unwrap();
        assert!(!old.active.load(Ordering::Acquire));
        tx.send(Vec::new()).unwrap();
        assert_eq!(guard.map("@bob:example.org", "BOB", "second", None), None);
        assert_eq!(guard.map("@bob:example.org", "BOB", "second", Some(&[])), None);
        assert_eq!(guard.map("@bob:example.org", "BOB", "first", Some(&[first])), None);
        assert!(current.active.load(Ordering::Acquire));
        assert!(guard.member_lease(identity, "second").is_some());
    }

    #[test]
    fn stable_member_id_can_return_with_a_fresh_irreversible_lease() {
        let guard = guard();
        let first = member("@bob:example.org", Some("BOB"), "stable");
        let second = member("@bob:example.org", Some("BOB"), "transient");
        let identity = "@bob:example.org:BOB";
        assert!(guard.on_core_snapshot(std::slice::from_ref(&first)));
        let a = guard.member_lease(identity, "stable").unwrap();
        assert!(guard.on_core_snapshot(&[second]));
        let b = guard.member_lease(identity, "transient").unwrap();
        assert!(guard.on_core_snapshot(&[first]));
        let returned = guard.member_lease(identity, "stable").unwrap();
        assert!(!a.active.load(Ordering::Acquire));
        assert!(!b.active.load(Ordering::Acquire));
        assert!(returned.active.load(Ordering::Acquire));
        assert!(!Arc::ptr_eq(&a.active, &returned.active));
        assert!(Arc::ptr_eq(&a.serial, &returned.serial));
        assert!(Arc::ptr_eq(&b.serial, &returned.serial));
        assert!(guard.active.load(Ordering::Acquire));
    }

    #[test]
    fn same_principal_rejoin_retires_old_generation_without_ending_call() {
        let guard = guard();
        let bob = member("@bob:example.org", Some("BOB"), "bob");
        let identity = "@bob:example.org:BOB";
        assert!(guard.on_core_snapshot(std::slice::from_ref(&bob)));
        let old = guard.member_lease(identity, "bob").unwrap();
        assert!(guard.on_core_snapshot(&[]));
        assert!(!old.active.load(Ordering::Acquire));
        let replacement = member("@bob:example.org", Some("BOB"), "replacement");
        assert!(guard.on_core_snapshot(std::slice::from_ref(&replacement)));
        let new = guard.member_lease(identity, "replacement").unwrap();
        assert!(Arc::ptr_eq(&old.serial, &new.serial));
        assert!(new.active.load(Ordering::Acquire));
        assert!(guard.active.load(Ordering::Acquire));
        assert_eq!(guard.map("@bob:example.org", "BOB", "bob", Some(&[replacement])), None);
        assert!(guard.admit(std::slice::from_ref(&bob)));
        assert_eq!(guard.map("@bob:example.org", "BOB", "bob", Some(&[bob])), None);
        assert_eq!(guard.map("@bob:example.org", "BOB", "replacement", Some(&[])), None);
        assert!(!old.active.load(Ordering::Acquire));
        assert!(new.active.load(Ordering::Acquire));
        assert!(guard.member_lease(identity, "bob").is_none());
    }

    #[test]
    fn direct_snapshot_handover_and_stale_own_membership_are_safe() {
        let guard = guard();
        let first = member("@bob:example.org", Some("BOB"), "first");
        assert!(guard.on_core_snapshot(&[first]));
        let old = guard.member_lease("@bob:example.org:BOB", "first").unwrap();
        let second = member("@bob:example.org", Some("BOB"), "second");
        let ghost = member("@alice:example.org", Some("ALICE"), "old-alice");
        assert!(!guard.include_member(&ghost));
        assert!(guard.on_core_snapshot(&[second, ghost.clone()]));
        assert!(!old.active.load(Ordering::Acquire));
        assert!(guard.member_lease("@bob:example.org:BOB", "second").is_some());
        assert_eq!(guard.map("@alice:example.org", "ALICE", "old-alice", Some(&[ghost])), None);
        assert!(guard.active.load(Ordering::Acquire));
    }

    #[test]
    fn a_legacy_alias_cannot_change_authenticated_principal() {
        let guard = guard();
        let first = member("@bob:example.org:8448", Some("DEVICE"), "first");
        let collision = member("@bob:example.org", Some("8448:DEVICE"), "second");
        assert!(guard.on_core_snapshot(&[first]));
        assert!(!guard.on_core_snapshot(&[collision]));
        assert!(!guard.active.load(Ordering::Acquire));
    }

    #[test]
    fn same_member_identity_change_and_missing_device_are_terminal() {
        let guard = guard();
        let bob = member("@bob:example.org", Some("BOB"), "bob");
        assert!(guard.on_core_snapshot(&[bob]));
        assert!(!guard.on_core_snapshot(&[member("@mallory:example.org", Some("BOB"), "bob")]));
        let missing = self::guard();
        assert!(!missing.on_core_snapshot(&[member("@bob:example.org", None, "bob")]));
    }

    #[test]
    fn same_owner_membership_refresh_remains_valid() {
        let guard = guard();
        let mut bob = member("@bob:example.org", Some("BOB"), "bob");
        bob.membership_ts = Some(1);
        assert!(guard.on_core_snapshot(std::slice::from_ref(&bob)));
        bob.membership_ts = Some(2);
        assert!(guard.on_core_snapshot(&[bob]));
    }
}
