//! Test-only, bounded counters for passive actor discovery and admission.
//! No identities, payloads, credentials, free-form errors or timing values.
//! The production worker has no diagnostic fields or counter operations.

use crate::incoming_ring::{AdmissionFailure, controller::ControllerFailure};
use std::sync::atomic::{AtomicU32, Ordering};

const LIMIT: u32 = 65_535;
#[derive(Clone, Copy)]
#[repr(usize)]
pub(super) enum Diagnostic {
    ScopeUnavailable,
    ScopeSetupFailed,
    GateInvalidation,
    GateShutdown,
    Publication,
    PublicationRejected,
    CandidateQueued,
    CandidateStaleEpoch,
    CandidateInterrupted,
    CandidateInvalidated,
    AdmissionStarted,
    AdmissionSucceeded,
    ControllerUnavailable,
    ControllerCapacity,
    ControllerStale,
    ControllerExpired,
    ControllerBusy,
    ControllerRevoked,
    ControllerWrongCall,
    AdmissionAccount,
    AdmissionSync,
    AdmissionReceipt,
    AdmissionReplay,
    AdmissionExpired,
    AdmissionMembership,
    AdmissionTrust,
    AdmissionPolicy,
    AdmissionQuiet,
    AdmissionTransport,
    AdmissionUnavailable,
    AdmissionTimedOut,
    AdmissionRevoked,
    CacheInvalidated,
    ReceiptCapacity,
    MissingIdentity,
    ReceiptOversized,
    ReceiptUnverified,
    ReceiptMalformed,
    ReceiptIdentity,
    NoVerifiedReceipts,
    MemberCapacity,
    MemberOversized,
    MemberMalformed,
    MemberUnverified,
    MemberMismatch,
    CallerMembershipMissing,
    CacheChanged,
    GateUnavailable,
    CandidateQueueFull,
    SeenRetained,
    MemberDeparted,
}

const NAMES: &[&str] = &[
    "worker_scope_unavailable",
    "worker_scope_setup_failed",
    "worker_gate_invalidation",
    "worker_gate_shutdown",
    "worker_publication",
    "worker_publication_rejected",
    "worker_candidate_queued",
    "worker_candidate_stale_epoch",
    "worker_candidate_interrupted",
    "worker_candidate_invalidated",
    "worker_admission_started",
    "worker_admission_succeeded",
    "worker_controller_unavailable",
    "worker_controller_capacity",
    "worker_controller_stale",
    "worker_controller_expired",
    "worker_controller_busy",
    "worker_controller_revoked",
    "worker_controller_wrong_call",
    "worker_admission_account",
    "worker_admission_sync",
    "worker_admission_receipt",
    "worker_admission_replay",
    "worker_admission_expired",
    "worker_admission_membership",
    "worker_admission_trust",
    "worker_admission_policy",
    "worker_admission_quiet",
    "worker_admission_transport",
    "worker_admission_unavailable",
    "worker_admission_timed_out",
    "worker_admission_revoked",
    "worker_cache_invalidated",
    "worker_receipt_capacity",
    "worker_missing_identity",
    "worker_receipt_oversized",
    "worker_receipt_unverified",
    "worker_receipt_malformed",
    "worker_receipt_identity",
    "worker_no_verified_receipts",
    "worker_member_capacity",
    "worker_member_oversized",
    "worker_member_malformed",
    "worker_member_unverified",
    "worker_member_mismatch",
    "worker_caller_membership_missing",
    "worker_cache_changed",
    "worker_gate_unavailable",
    "worker_candidate_queue_full",
    "worker_seen_retained",
    "worker_member_departed",
];

pub(super) struct Counters([AtomicU32; NAMES.len()]);
impl Default for Counters {
    fn default() -> Self {
        Self(std::array::from_fn(|_| AtomicU32::new(0)))
    }
}
impl Counters {
    pub(super) fn record(&self, value: Diagnostic) {
        let _ = self.0[value as usize].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            Some(old.saturating_add(1).min(LIMIT))
        });
    }
    pub(super) fn result<T>(&self, result: &Result<T, ControllerFailure>) {
        use Diagnostic::*;
        self.record(match result {
            Ok(_) => AdmissionSucceeded,
            Err(ControllerFailure::Unavailable) => ControllerUnavailable,
            Err(ControllerFailure::Capacity) => ControllerCapacity,
            Err(ControllerFailure::Stale) => ControllerStale,
            Err(ControllerFailure::Expired) => ControllerExpired,
            Err(ControllerFailure::Busy) => ControllerBusy,
            Err(ControllerFailure::Revoked) => ControllerRevoked,
            Err(ControllerFailure::WrongCall) => ControllerWrongCall,
            Err(ControllerFailure::Admission(value)) => match value {
                AdmissionFailure::Account => AdmissionAccount,
                AdmissionFailure::Sync => AdmissionSync,
                AdmissionFailure::Receipt => AdmissionReceipt,
                AdmissionFailure::Replay => AdmissionReplay,
                AdmissionFailure::Expired => AdmissionExpired,
                AdmissionFailure::Membership => AdmissionMembership,
                AdmissionFailure::Trust => AdmissionTrust,
                AdmissionFailure::Policy => AdmissionPolicy,
                AdmissionFailure::Quiet => AdmissionQuiet,
                AdmissionFailure::Transport => AdmissionTransport,
                AdmissionFailure::Unavailable => AdmissionUnavailable,
                AdmissionFailure::TimedOut => AdmissionTimedOut,
                AdmissionFailure::Revoked => AdmissionRevoked,
            },
        });
    }
    pub(super) fn snapshot(&self) -> Snapshot {
        Snapshot(std::array::from_fn(|i| self.0[i].load(Ordering::Relaxed)))
    }
}

pub(crate) struct Snapshot([u32; NAMES.len()]);
impl Snapshot {
    fn delta(&self, before: &Self, i: usize) -> u32 {
        self.0[i].saturating_sub(before.0[i])
    }
    pub(crate) fn emit_since(&self, before: &Self) {
        // A bounded fixed alphabet, emitted only by a failing fixture. Counts
        // are observations, never permission to retry/rearm an old receipt.
        for (i, name) in NAMES.iter().enumerate() {
            eprintln!(
                "\nMORPHEUS_INCOMING_DIAGNOSTIC {name}={}",
                self.delta(before, i)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_per_worker_and_deltas_do_not_reset_history() {
        let first = Counters::default();
        let second = Counters::default();
        first.record(Diagnostic::CandidateQueued);
        let before = first.snapshot();
        first.record(Diagnostic::CandidateQueued);
        first.result::<()>(&Err(ControllerFailure::Admission(
            AdmissionFailure::Receipt,
        )));
        let after = first.snapshot();
        assert_eq!(
            after.delta(&before, Diagnostic::CandidateQueued as usize),
            1
        );
        assert_eq!(
            after.delta(&before, Diagnostic::AdmissionReceipt as usize),
            1
        );
        assert_eq!(first.snapshot().0[Diagnostic::CandidateQueued as usize], 2);
        assert!(second.snapshot().0.into_iter().all(|n| n == 0));
    }

    #[test]
    fn diagnostic_counts_saturate_without_wrapping_or_subtraction_underflow() {
        let counters = Counters::default();
        counters.0[Diagnostic::CacheChanged as usize].store(LIMIT, Ordering::Relaxed);
        counters.record(Diagnostic::CacheChanged);
        let saturated = counters.snapshot();
        assert_eq!(saturated.0[Diagnostic::CacheChanged as usize], LIMIT);
        assert_eq!(
            Counters::default()
                .snapshot()
                .delta(&saturated, Diagnostic::CacheChanged as usize),
            0
        );
    }
}
