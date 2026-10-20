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
    collections::{VecDeque, hash_map::RandomState},
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
    fn next_attempt_with_jitter(&mut self, now: Instant, jitter_permille: u64) -> Option<Duration> {
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

/// Bounds how often one call may start recovering, so a connection that
/// drops again right after each successful rejoin ends instead of cycling.
#[derive(Clone, Debug)]
pub struct ReconnectWindow {
    limit: usize,
    window: Duration,
    starts: VecDeque<Instant>,
}

impl ReconnectWindow {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            starts: VecDeque::new(),
        }
    }

    /// The native call controller's limit: three recoveries in ten minutes.
    pub fn per_call() -> Self {
        Self::new(3, Duration::from_secs(600))
    }

    /// Record a recovery starting at `now`, or refuse it when `limit`
    /// recoveries already started within the window.
    pub fn try_start(&mut self, now: Instant) -> bool {
        while let Some(&start) = self.starts.front() {
            if now.saturating_duration_since(start) < self.window {
                break;
            }
            self.starts.pop_front();
        }
        if self.starts.len() >= self.limit {
            return false;
        }
        self.starts.push_back(now);
        true
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
    /// The SFU deliberately removed this participant: an administrator
    /// removed it, deleted the room, or the same identity joined elsewhere.
    /// Rejoining automatically would undo that decision.
    Removed,
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
                    EndedReason::ConnectionClosed { message } if removed_by_server(message) => {
                        RtcCallEnd::Removed
                    }
                    EndedReason::ConnectionClosed { .. } => RtcCallEnd::ConnectionLost,
                };
                Some(Self::Ended(end))
            }
            _ => None,
        }
    }
}

/// The pinned call SDK reports the transport's disconnect reason only as the
/// `Debug` name of LiveKit's `DisconnectReason`. A server shutdown, timeout,
/// migration or signalling failure is not a removal and may be rejoined.
#[cfg(feature = "native-rtc")]
fn removed_by_server(message: &str) -> bool {
    matches!(
        message,
        "ParticipantRemoved" | "RoomDeleted" | "DuplicateIdentity"
    )
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

    #[cfg(feature = "native-rtc")]
    #[test]
    fn deliberate_removals_are_not_treated_as_connection_loss() {
        use matrix_rtc_call_sdk::{CallEvent, EndedReason};

        let ended = |message: &str| {
            RtcConnectionSignal::from_event(&CallEvent::Ended {
                reason: EndedReason::ConnectionClosed {
                    message: message.to_owned(),
                },
            })
        };
        for removed in ["ParticipantRemoved", "RoomDeleted", "DuplicateIdentity"] {
            assert_eq!(
                ended(removed),
                Some(RtcConnectionSignal::Ended(RtcCallEnd::Removed))
            );
        }
        for lost in [
            "ServerShutdown",
            "SignalClose",
            "failed to reconnect after 10",
        ] {
            assert_eq!(
                ended(lost),
                Some(RtcConnectionSignal::Ended(RtcCallEnd::ConnectionLost))
            );
        }
    }

    #[test]
    fn recoveries_are_limited_within_the_window() {
        let start = Instant::now();
        let mut window = ReconnectWindow::new(2, Duration::from_secs(60));
        assert!(window.try_start(start));
        assert!(window.try_start(start + Duration::from_secs(10)));
        assert!(!window.try_start(start + Duration::from_secs(20)));
        assert!(window.try_start(start + Duration::from_secs(61)));
        assert!(!window.try_start(start + Duration::from_secs(62)));
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
