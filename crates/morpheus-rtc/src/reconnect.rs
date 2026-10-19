//! Bounded recovery around one native MatrixRTC call.
//!
//! The pinned LiveKit engine resumes, then fully reconnects, its own SFU
//! session inside a single MatrixRTC join (at most ten attempts, five seconds
//! apart) and republishes the same local tracks itself. The call engine only
//! ends the call once the focus this device publishes on is gone for good.
//! These helpers classify those connection signals and bound what the host
//! does afterwards: one explicit leave, then a limited number of fresh joins.
//! Every fresh join repeats transport discovery, encryption readiness and the
//! patched transport's HTTPS authorization and WSS media endpoint checks; no
//! retry reuses an old token, endpoint or membership.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    time::{Duration, Instant},
};

/// Limits for rejoining after a call's own media connection ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconnectPolicy {
    /// Fresh joins attempted at most, including the first.
    pub max_attempts: u32,
    /// Nominal wait before the first attempt; doubles for each further one.
    pub initial_delay: Duration,
    /// Upper bound for one nominal wait.
    pub max_delay: Duration,
    /// Wall-clock bound from the interruption, including any wait for the
    /// homeserver to come back. No attempt starts after it.
    pub budget: Duration,
}

impl ReconnectPolicy {
    /// The native call controller's rejoin policy. The budget stays below the
    /// controller's shutdown wait so a Leave during recovery is never cut
    /// short by its own timeout.
    pub const fn rejoin() -> Self {
        Self {
            max_attempts: 4,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(15),
            budget: Duration::from_secs(60),
        }
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self::rejoin()
    }
}

/// Attempt and deadline accounting for one interruption. A new interruption
/// needs a new value; it never resets itself.
#[derive(Clone, Debug)]
pub struct ReconnectBackoff {
    policy: ReconnectPolicy,
    started: Instant,
    attempts: u32,
}

impl ReconnectBackoff {
    pub fn new(policy: ReconnectPolicy, started: Instant) -> Self {
        Self {
            policy,
            started,
            attempts: 0,
        }
    }

    pub fn policy(&self) -> ReconnectPolicy {
        self.policy
    }

    /// Attempts reserved so far.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn deadline(&self) -> Instant {
        self.started
            .checked_add(self.policy.budget)
            .unwrap_or(self.started)
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.deadline().saturating_duration_since(now)
    }

    pub fn expired(&self, now: Instant) -> bool {
        now >= self.deadline()
    }

    /// Reserve the next attempt and return how long to wait before starting
    /// it, or `None` once the attempts or the budget are used up.
    pub fn next_attempt(&mut self, now: Instant) -> Option<Duration> {
        self.next_attempt_with_jitter(now, random_permille(self.attempts))
    }

    /// `jitter_permille` (0..=1000) places the wait between 80 % and 100 % of
    /// the nominal delay, so clients interrupted together do not retry in step.
    fn next_attempt_with_jitter(
        &mut self,
        now: Instant,
        jitter_permille: u64,
    ) -> Option<Duration> {
        if self.attempts >= self.policy.max_attempts || self.expired(now) {
            return None;
        }
        let factor = 1_u32.checked_shl(self.attempts).unwrap_or(u32::MAX);
        let nominal = self
            .policy
            .initial_delay
            .checked_mul(factor)
            .unwrap_or(self.policy.max_delay)
            .min(self.policy.max_delay);
        let scale = u128::from(800 + jitter_permille.min(1000) / 5);
        let millis = nominal.as_millis() * scale / 1000;
        let delay = Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX));
        if now
            .checked_add(delay)
            .is_none_or(|start| start >= self.deadline())
        {
            return None;
        }
        self.attempts += 1;
        Some(delay)
    }
}

fn random_permille(salt: u32) -> u64 {
    // std's per-instance random SipHash keys avoid adding an RNG dependency.
    // Jitter only spreads retries; nothing security-relevant depends on it.
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(salt);
    hasher.finish() % 1001
}

/// Why a joined call ended, in Morpheus's vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RtcCallEnd {
    /// The call session ended without a closed slot: this device left, or
    /// its membership ended without it (e.g. its delayed leave fired).
    Left,
    /// Someone with room power closed the call slot for everyone.
    SlotClosed,
    /// The SFU connection this device publishes on closed and the transport
    /// gave up reconnecting it. The membership may still be published.
    ConnectionLost,
}

/// The connection-health subset of a call's event stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RtcConnectionSignal {
    /// The transport is resuming or restarting a media connection.
    MediaDegraded,
    /// Every media connection is up again after a degraded period.
    MediaRestored,
    /// No further events follow.
    Ended(RtcCallEnd),
}

#[cfg(feature = "native-rtc")]
impl RtcConnectionSignal {
    /// Map one unified call event; `None` for events about participants,
    /// streams, keys or reactions.
    pub fn from_event(event: &crate::RtcCallEvent) -> Option<Self> {
        use matrix_rtc_call_sdk::{CallEvent, EndedReason};
        match event {
            CallEvent::MediaConnectionState { degraded: true } => Some(Self::MediaDegraded),
            CallEvent::MediaConnectionState { degraded: false } => Some(Self::MediaRestored),
            CallEvent::Ended { reason } => {
                let end = match reason {
                    EndedReason::Left => RtcCallEnd::Left,
                    EndedReason::SlotClosed => RtcCallEnd::SlotClosed,
                    EndedReason::ConnectionClosed { .. } => RtcCallEnd::ConnectionLost,
                };
                Some(Self::Ended(end))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts: 4,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            budget: Duration::from_secs(60),
        }
    }

    #[test]
    fn delays_double_up_to_the_cap_and_attempts_are_bounded() {
        let start = Instant::now();
        let mut backoff = ReconnectBackoff::new(policy(), start);
        let delays: Vec<_> = (0..5)
            .map(|_| backoff.next_attempt_with_jitter(start, 1000))
            .collect();
        assert_eq!(
            delays,
            vec![
                Some(Duration::from_secs(1)),
                Some(Duration::from_secs(2)),
                Some(Duration::from_secs(4)),
                Some(Duration::from_secs(5)),
                None,
            ]
        );
        assert_eq!(backoff.attempts(), 4);
    }

    #[test]
    fn jitter_only_shortens_the_nominal_delay_by_at_most_a_fifth() {
        let start = Instant::now();
        let mut low = ReconnectBackoff::new(policy(), start);
        assert_eq!(
            low.next_attempt_with_jitter(start, 0),
            Some(Duration::from_millis(800))
        );
        let mut high = ReconnectBackoff::new(policy(), start);
        assert_eq!(
            high.next_attempt_with_jitter(start, 5000),
            Some(Duration::from_secs(1))
        );
        let mut random = ReconnectBackoff::new(policy(), start);
        let delay = random.next_attempt(start).unwrap();
        assert!(delay >= Duration::from_millis(800) && delay <= Duration::from_secs(1));
    }

    #[test]
    fn no_attempt_starts_at_or_after_the_budget() {
        let start = Instant::now();
        let mut backoff = ReconnectBackoff::new(policy(), start);
        let late = start + Duration::from_millis(59_500);
        assert_eq!(backoff.next_attempt_with_jitter(late, 1000), None);
        assert_eq!(backoff.attempts(), 0);
        assert!(!backoff.expired(late));
        assert!(backoff.expired(start + Duration::from_secs(60)));
        assert_eq!(
            backoff.remaining(start + Duration::from_secs(70)),
            Duration::ZERO
        );
    }
}
