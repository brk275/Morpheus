//! Process-lifetime replay protection for incoming call offers.
//!
//! The controller owns one ledger across account selection changes. Call this
//! only after the receipt is a bounded, encrypted notification matching the
//! selected account/room, before asynchronous admission work. An admission
//! failure or dismissal must retire its key. A final semantic ring window may
//! shorten the offer's deadline; it must never extend the deadline stored here.
//!
//! No entries are evicted, even after expiry, logout or account switching. A
//! replay after a wall-clock rollback therefore cannot obtain a fresh offer.
//! Reaching the capacity makes all new keys ineligible until process restart;
//! this is an intentional availability limit, not a reason to clear history.
//! This memory-only ledger does not claim replay protection across restarts.

use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, Instant},
};

use morpheus_rtc::ringing::{MAX_RING_LIFETIME_MS, RingWindow};

const DEFAULT_CAPACITY: usize = 1024;
const MAX_KEY_COMPONENT_BYTES: usize = 255;

/// The account is the stable local account ID, not a selection generation.
/// Returning A -> B -> A must address the same entry for A.
#[derive(Clone, Copy)]
pub(crate) struct OfferKey<'a> {
    account_id: &'a str,
    room_id: &'a str,
    event_id: &'a str,
}

impl<'a> OfferKey<'a> {
    pub(crate) fn new(account_id: &'a str, room_id: &'a str, event_id: &'a str) -> Self {
        Self {
            account_id,
            room_id,
            event_id,
        }
    }

    fn owned(self) -> Result<StoredKey, LedgerError> {
        let valid = |part: &str| !part.is_empty() && part.len() <= MAX_KEY_COMPONENT_BYTES;
        if !valid(self.account_id) || !valid(self.room_id) || !valid(self.event_id) {
            return Err(LedgerError::InvalidKey);
        }
        Ok(StoredKey {
            account_id: self.account_id.to_owned(),
            room_id: self.room_id.to_owned(),
            event_id: self.event_id.to_owned(),
        })
    }
}

impl fmt::Debug for OfferKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfferKey").finish_non_exhaustive()
    }
}

// Intentionally no Debug implementation: the ledger exposes counts only.
#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct StoredKey {
    account_id: String,
    room_id: String,
    event_id: String,
}

struct Entry {
    // None is an early dismissal or a failed checked_add, never an offer.
    deadline: Option<Instant>,
    retired: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Observation {
    /// The sole opportunity to start admission for this key.
    FirstSeen {
        deadline: Instant,
    },
    Replay,
    Expired,
    Retired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LedgerError {
    InvalidKey,
    InvalidCapacity,
    CapacityExceeded,
    DeadlineOverflow,
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidKey => "invalid incoming offer key",
            Self::InvalidCapacity => "invalid incoming offer ledger capacity",
            Self::CapacityExceeded => "incoming offer replay ledger is full until restart",
            Self::DeadlineOverflow => "incoming offer deadline is out of range",
        })
    }
}

impl std::error::Error for LedgerError {}

pub(crate) struct OfferLedger {
    entries: BTreeMap<StoredKey, Entry>,
    capacity: usize,
    saturated: bool,
}

impl Default for OfferLedger {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            capacity: DEFAULT_CAPACITY,
            saturated: false,
        }
    }
}

impl fmt::Debug for OfferLedger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfferLedger")
            .field("entry_count", &self.entries.len())
            .field("capacity", &self.capacity)
            .field("saturated", &self.saturated)
            .finish_non_exhaustive()
    }
}

impl OfferLedger {
    /// Smaller capacities are useful for constrained hosts and focused tests.
    /// Zero deliberately disables new offers; no value may exceed the cap.
    pub(crate) fn with_capacity(capacity: usize) -> Result<Self, LedgerError> {
        if capacity > DEFAULT_CAPACITY {
            return Err(LedgerError::InvalidCapacity);
        }
        Ok(Self {
            capacity,
            ..Self::default()
        })
    }

    /// Reserve a notification exactly once before asynchronous admission.
    /// `window` is the conservative retained-receipt window at first sight.
    /// Its wall-clock expiry is intentionally never used to rearm a key.
    pub(crate) fn observe(
        &mut self,
        key: OfferKey<'_>,
        window: RingWindow,
        now: Instant,
    ) -> Result<Observation, LedgerError> {
        let key = key.owned()?;
        if let Some(entry) = self.entries.get(&key) {
            return Ok(if entry.retired {
                Observation::Retired
            } else if entry.deadline.is_none_or(|deadline| now >= deadline) {
                Observation::Expired
            } else {
                Observation::Replay
            });
        }
        self.reserve()?;
        let remaining = Duration::from_millis(window.remaining_ms.min(MAX_RING_LIFETIME_MS));
        let deadline = now.checked_add(remaining);
        // A failed time calculation is itself remembered, so retrying this
        // same event with different clock inputs cannot create an offer.
        self.entries.insert(
            key,
            Entry {
                deadline,
                retired: deadline.is_none(),
            },
        );
        let deadline = deadline.ok_or(LedgerError::DeadlineOverflow)?;
        Ok(if remaining.is_zero() {
            Observation::Expired
        } else {
            Observation::FirstSeen { deadline }
        })
    }

    /// Permanently prevent this notification from being offered again.
    /// Unknown keys get a bounded tombstone as well, for an early dismissal.
    pub(crate) fn retire(&mut self, key: OfferKey<'_>) -> Result<(), LedgerError> {
        let key = key.owned()?;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.retired = true;
            return Ok(());
        }
        self.reserve()?;
        self.entries.insert(
            key,
            Entry {
                deadline: None,
                retired: true,
            },
        );
        Ok(())
    }

    /// The original upper bound, including after retirement or expiry.
    pub(crate) fn deadline(&self, key: OfferKey<'_>) -> Option<Instant> {
        self.entries.get(&key.owned().ok()?)?.deadline
    }

    fn reserve(&mut self) -> Result<(), LedgerError> {
        if self.saturated || self.entries.len() >= self.capacity {
            self.saturated = true;
            return Err(LedgerError::CapacityExceeded);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> OfferKey<'static> {
        OfferKey::new("local-account-a", "!room:example.org", "$event")
    }

    fn window(remaining_ms: u64) -> RingWindow {
        RingWindow {
            expires_at_ms: 1_000_000 + remaining_ms,
            remaining_ms,
        }
    }

    #[test]
    fn first_deadline_survives_replays_and_wall_clock_rollback() {
        let mut ledger = OfferLedger::default();
        let start = Instant::now();
        let deadline = start + Duration::from_secs(30);
        assert_eq!(
            ledger.observe(key(), window(30_000), start),
            Ok(Observation::FirstSeen { deadline })
        );
        // A backwards wall clock makes the same absolute expiry look much
        // farther away. Even replacing that expiry cannot rearm the key.
        for expires_at_ms in [1, 1_030_000, u64::MAX] {
            assert_eq!(
                ledger.observe(
                    key(),
                    RingWindow {
                        expires_at_ms,
                        remaining_ms: u64::MAX,
                    },
                    start + Duration::from_secs(1),
                ),
                Ok(Observation::Replay)
            );
            assert_eq!(ledger.deadline(key()), Some(deadline));
        }
        assert_eq!(
            ledger.observe(key(), window(120_000), deadline),
            Ok(Observation::Expired)
        );
        assert_eq!(ledger.deadline(key()), Some(deadline));
    }

    #[test]
    fn first_window_is_capped_and_zero_window_never_rearms() {
        let mut ledger = OfferLedger::default();
        let now = Instant::now();
        assert_eq!(
            ledger.observe(key(), window(u64::MAX - 1_000_000), now),
            Ok(Observation::FirstSeen {
                deadline: now + Duration::from_millis(MAX_RING_LIFETIME_MS),
            })
        );
        let expired = OfferKey::new("local-account-a", "!room:example.org", "$expired");
        assert_eq!(
            ledger.observe(expired, window(0), now),
            Ok(Observation::Expired)
        );
        assert_eq!(
            ledger.observe(expired, window(30_000), now),
            Ok(Observation::Expired)
        );
    }

    #[test]
    fn dismissal_and_a_to_b_to_a_preserve_history() {
        let mut ledger = OfferLedger::default();
        let now = Instant::now();
        let a = key();
        let b = OfferKey::new("local-account-b", "!room:example.org", "$event");
        let deadline = now + Duration::from_secs(30);
        assert_eq!(
            ledger.observe(a, window(30_000), now),
            Ok(Observation::FirstSeen { deadline })
        );
        ledger.retire(a).unwrap();
        assert_eq!(
            ledger.observe(b, window(30_000), now),
            Ok(Observation::FirstSeen { deadline })
        );
        assert_eq!(
            ledger.observe(a, window(120_000), now),
            Ok(Observation::Retired)
        );
        assert_eq!(ledger.deadline(a), Some(deadline));
        assert_eq!(
            ledger.observe(b, window(120_000), now),
            Ok(Observation::Replay)
        );
    }

    #[test]
    fn room_and_event_keys_are_distinct_without_delimiter_collisions() {
        let mut ledger = OfferLedger::default();
        let now = Instant::now();
        for item in [
            key(),
            OfferKey::new("local-account-a", "!other:example.org", "$event"),
            OfferKey::new("local-account-a", "!room:example.org", "$other"),
            OfferKey::new("a/b", "c", "d"),
            OfferKey::new("a", "b/c", "d"),
        ] {
            assert!(matches!(
                ledger.observe(item, window(30_000), now),
                Ok(Observation::FirstSeen { .. })
            ));
        }
        assert_eq!(ledger.entries.len(), 5);
    }

    #[test]
    fn overflow_latches_without_evicting_expired_or_retired_entries() {
        let mut ledger = OfferLedger::with_capacity(2).unwrap();
        let now = Instant::now();
        let retired = OfferKey::new("local-account-a", "!room:example.org", "$retired");
        let extra = OfferKey::new("local-account-a", "!room:example.org", "$extra");
        assert_eq!(
            ledger.observe(key(), window(0), now),
            Ok(Observation::Expired)
        );
        ledger.retire(retired).unwrap();
        assert_eq!(
            ledger.observe(extra, window(30_000), now),
            Err(LedgerError::CapacityExceeded)
        );
        assert!(ledger.saturated);
        assert_eq!(ledger.entries.len(), 2);
        ledger.retire(key()).unwrap();
        assert_eq!(
            ledger.observe(key(), window(30_000), now),
            Ok(Observation::Retired)
        );
        assert_eq!(
            ledger.observe(retired, window(30_000), now),
            Ok(Observation::Retired)
        );
        assert_eq!(ledger.retire(extra), Err(LedgerError::CapacityExceeded));
        assert_eq!(
            ledger.observe(extra, window(30_000), now + Duration::from_secs(600)),
            Err(LedgerError::CapacityExceeded)
        );
    }

    #[test]
    fn early_retirement_blocks_first_observation() {
        let mut ledger = OfferLedger::default();
        ledger.retire(key()).unwrap();
        assert_eq!(ledger.deadline(key()), None);
        assert_eq!(
            ledger.observe(key(), window(30_000), Instant::now()),
            Ok(Observation::Retired)
        );
    }

    #[test]
    fn checked_deadline_overflow_retires_the_attempt() {
        let mut ledger = OfferLedger::default();
        let now = Instant::now();
        // Find this platform's representable boundary without sleeping or
        // assuming that Instant uses the same underlying clock on every OS.
        let mut boundary = now;
        for shift in (0..64).rev() {
            if let Some(later) = boundary.checked_add(Duration::from_secs(1_u64 << shift)) {
                boundary = later;
            }
        }
        assert!(boundary.checked_add(Duration::from_secs(30)).is_none());
        assert_eq!(
            ledger.observe(key(), window(30_000), boundary),
            Err(LedgerError::DeadlineOverflow)
        );
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.deadline(key()), None);
        assert_eq!(
            ledger.observe(key(), window(30_000), now),
            Ok(Observation::Retired)
        );
    }

    #[test]
    fn invalid_keys_and_capacities_do_not_allocate_history() {
        let mut ledger = OfferLedger::default();
        let now = Instant::now();
        let oversized = "x".repeat(MAX_KEY_COMPONENT_BYTES + 1);
        for invalid in [
            OfferKey::new("", "room", "event"),
            OfferKey::new("account", "", "event"),
            OfferKey::new("account", "room", ""),
            OfferKey::new(&oversized, "room", "event"),
            OfferKey::new("account", &oversized, "event"),
            OfferKey::new("account", "room", &oversized),
        ] {
            assert_eq!(
                ledger.observe(invalid, window(30_000), now),
                Err(LedgerError::InvalidKey)
            );
            assert_eq!(ledger.retire(invalid), Err(LedgerError::InvalidKey));
            assert_eq!(ledger.deadline(invalid), None);
        }
        assert!(ledger.entries.is_empty());
        assert!(!ledger.saturated);
        assert!(matches!(
            OfferLedger::with_capacity(DEFAULT_CAPACITY + 1),
            Err(LedgerError::InvalidCapacity)
        ));
        assert_eq!(
            OfferLedger::with_capacity(0)
                .unwrap()
                .observe(key(), window(30_000), now),
            Err(LedgerError::CapacityExceeded)
        );
    }

    #[test]
    fn debug_and_errors_do_not_expose_account_room_or_event_ids() {
        let mut ledger = OfferLedger::default();
        ledger.retire(key()).unwrap();
        let debug = format!("{ledger:?} {:?}", key());
        for private in [key().account_id, key().room_id, key().event_id] {
            assert!(!debug.contains(private));
            for error in [
                LedgerError::InvalidKey,
                LedgerError::InvalidCapacity,
                LedgerError::CapacityExceeded,
                LedgerError::DeadlineOverflow,
            ] {
                assert!(!format!("{error:?}: {error}").contains(private));
            }
        }
    }
}
