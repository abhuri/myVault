//! Sealed, fail-closed orchestration for local-execution finalization.
//!
//! This module deliberately contains no Desktop, Android, SAF, provider, or
//! filesystem mutation adapter.  A final outcome is possible only after a
//! platform-specific verifier (which does not exist in production yet) issues
//! opaque evidence through this crate's sealed trust boundary.

use std::fmt;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    local_identity::DurableExecutionBinding, LocalExecutionAttemptBoundary, LocalExecutionOutcome,
    LocalExecutionRecoveryObservation, Result, SyncStore,
};

const OUTCOME_ID_DOMAIN: &[u8] = b"myvault-r3.5-authoritative-outcome-v1";
const LOCAL_EVIDENCE_DOMAIN: &[u8] = b"myvault-r3.5-authoritative-local-evidence-v1";

mod verifier_seal {
    pub trait Sealed {}
}

/// Whether the platform call boundary was observed.  This is intentionally
/// distinct from the independently observed final state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformCallFact {
    NotEntered,
    Returned,
    Ambiguous,
}

/// Opaque, verifier-issued facts for one exact durable execution attempt.
///
/// There is no public constructor and no production verifier in R3.5.  This
/// keeps watcher observations, `SQLite` rows, journal claims, and caller input
/// from becoming final-outcome authority.
#[derive(Clone, Copy)]
pub struct AuthoritativeFinalEvidence {
    operation_id: Uuid,
    attempt_number: u32,
    intent_fingerprint: [u8; 32],
    contract_fingerprint: [u8; 32],
    collision_snapshot_fingerprint: [u8; 32],
    boundary_id: Uuid,
    boundary_occurred_at_unix_ms: u64,
    evidence_id: Uuid,
    /// Fingerprint of the R3 post-verify evidence.  This is deliberately not
    /// the local authoritative evidence fingerprint persisted in the v6 ledger.
    r3_mutation_evidence_fingerprint: [u8; 32],
    recorded_at_unix_ms: u64,
    call_fact: PlatformCallFact,
    verification: BindingVerificationFact,
    side_effect: SideEffectFact,
    final_state: FinalStateFact,
}

/// Exactness of the verifier's binding and collision revalidation.
#[allow(dead_code)] // R3.5 intentionally has no production verifier yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingVerificationFact {
    Exact,
    Unsupported,
    BindingMismatch,
    CollisionMismatch,
    Substituted,
    Inconsistent,
}

/// Whether the verifier independently observed a forbidden side effect.
#[allow(dead_code)] // R3.5 intentionally has no production verifier yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SideEffectFact {
    None,
    Forbidden,
}

/// Full final-state verdict from the sealed verifier.
#[allow(dead_code)] // R3.5 intentionally has no production verifier yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinalStateFact {
    ExactApplied,
    ExactNotApplied,
    Insufficient,
    Contradictory,
}

impl fmt::Debug for AuthoritativeFinalEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthoritativeFinalEvidence")
            .field("operation_id", &self.operation_id)
            .field("attempt_number", &self.attempt_number)
            .field("call_fact", &self.call_fact)
            .field("verification", &self.verification)
            .field("side_effect", &self.side_effect)
            .field("final_state", &self.final_state)
            .field("bound_fingerprints", &"<redacted>")
            .field("evidence", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Trust boundary for a platform/provider final-state verifier.
///
/// R3.5 deliberately ships no production implementor.  Any future adapter
/// must be reviewed in this crate before it can issue final evidence; Android
/// SAF Gate 4 capability is explicitly not claimed here.
pub trait AuthoritativeLocalExecutionVerifier: verifier_seal::Sealed {
    /// Independently observes the exact bound state and issues opaque evidence.
    ///
    /// # Errors
    /// Returns a redacted error when authoritative revalidation cannot be
    /// completed.  Callers must not infer an outcome from that error.
    fn verify_final_state(
        &self,
        binding: &DurableExecutionBinding,
        boundary: &LocalExecutionAttemptBoundary,
    ) -> Result<AuthoritativeFinalEvidence>;
}

/// A final decision that can only originate from sealed authoritative evidence.
#[derive(Clone, Copy)]
pub struct AuthoritativeFinalOutcome {
    outcome: LocalExecutionOutcome,
    operation_id: Uuid,
    attempt_number: u32,
    intent_fingerprint: [u8; 32],
    contract_fingerprint: [u8; 32],
    collision_snapshot_fingerprint: [u8; 32],
    boundary_id: Uuid,
    boundary_occurred_at_unix_ms: u64,
    evidence_id: Uuid,
    r3_mutation_evidence_fingerprint: [u8; 32],
    evidence_fingerprint: [u8; 32],
    outcome_id: Uuid,
    recorded_at_unix_ms: u64,
}

impl fmt::Debug for AuthoritativeFinalOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthoritativeFinalOutcome")
            .field("outcome", &self.outcome)
            .field("operation_id", &self.operation_id)
            .field("attempt_number", &self.attempt_number)
            .field("outcome_id", &self.outcome_id)
            .field("evidence", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl AuthoritativeFinalOutcome {
    /// Returns the verifier-derived final classification.
    #[must_use]
    pub const fn outcome(&self) -> LocalExecutionOutcome {
        self.outcome
    }

    pub(crate) const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    pub(crate) const fn attempt_number(&self) -> u32 {
        self.attempt_number
    }

    pub(crate) const fn evidence_id(&self) -> Uuid {
        self.evidence_id
    }

    pub(crate) const fn boundary_id(&self) -> Uuid {
        self.boundary_id
    }

    pub(crate) const fn boundary_occurred_at_unix_ms(&self) -> u64 {
        self.boundary_occurred_at_unix_ms
    }

    pub(crate) const fn evidence_fingerprint(&self) -> [u8; 32] {
        self.evidence_fingerprint
    }

    pub(crate) const fn r3_mutation_evidence_fingerprint(&self) -> [u8; 32] {
        self.r3_mutation_evidence_fingerprint
    }

    pub(crate) const fn outcome_id(&self) -> Uuid {
        self.outcome_id
    }

    pub(crate) const fn recorded_at_unix_ms(&self) -> u64 {
        self.recorded_at_unix_ms
    }

    pub(crate) fn matches_binding(
        &self,
        binding: &DurableExecutionBinding,
        boundary: &LocalExecutionAttemptBoundary,
    ) -> bool {
        let projection = binding.persistence_projection();
        self.operation_id == projection.operation_id
            && self.attempt_number == boundary.attempt_number
            && self.intent_fingerprint == projection.intent_fingerprint
            && self.contract_fingerprint == projection.contract_fingerprint
            && self.collision_snapshot_fingerprint == projection.collision_snapshot_fingerprint
            && boundary.operation_id == projection.operation_id
            && self.boundary_id == boundary.boundary_id
            && self.boundary_occurred_at_unix_ms == boundary.occurred_at_unix_ms
            && *boundary.contract_fingerprint.as_bytes() == projection.contract_fingerprint
    }
}

/// Classifies only sealed verifier evidence for the supplied exact binding.
///
/// Claims in the journal or ledger cannot enter this function and therefore
/// cannot create `VerifiedApplied` or `VerifiedNotApplied`.
///
/// # Errors
///
/// Returns an error before hashing when IDs are nil or the evidence timestamp
/// cannot be represented by the durable `SQLite` ledger.
pub fn classify_authoritative_final_outcome(
    binding: &DurableExecutionBinding,
    boundary: &LocalExecutionAttemptBoundary,
    evidence: AuthoritativeFinalEvidence,
) -> Result<AuthoritativeFinalOutcome> {
    // Reject malformed evidence before deriving any fingerprint or durable
    // identifier. A verifier may never use nil IDs or an unrepresentable
    // timestamp as a domain-separation input.
    if evidence.operation_id.is_nil()
        || evidence.evidence_id.is_nil()
        || evidence.boundary_id.is_nil()
        || boundary.operation_id.is_nil()
        || boundary.boundary_id.is_nil()
    {
        return Err(crate::Error::InvalidLocalExecutionEvidence);
    }
    if evidence.recorded_at_unix_ms > i64::MAX as u64
        || evidence.boundary_occurred_at_unix_ms > i64::MAX as u64
        || boundary.occurred_at_unix_ms > i64::MAX as u64
    {
        return Err(crate::Error::InvalidTimestamp);
    }
    let projection = binding.persistence_projection();
    let exact_binding = evidence.operation_id == projection.operation_id
        && evidence.attempt_number == boundary.attempt_number
        && evidence.intent_fingerprint == projection.intent_fingerprint
        && evidence.contract_fingerprint == projection.contract_fingerprint
        && evidence.collision_snapshot_fingerprint == projection.collision_snapshot_fingerprint
        && evidence.boundary_id == boundary.boundary_id
        && evidence.boundary_occurred_at_unix_ms == boundary.occurred_at_unix_ms
        && boundary.operation_id == projection.operation_id
        && *boundary.contract_fingerprint.as_bytes() == projection.contract_fingerprint;
    let outcome = if !exact_binding
        || evidence.verification != BindingVerificationFact::Exact
        || evidence.side_effect != SideEffectFact::None
    {
        LocalExecutionOutcome::NeedsReconcile
    } else {
        match (evidence.call_fact, evidence.final_state) {
            // A verifier cannot claim an operation was applied when it also
            // observed that the side-effect boundary was never entered.
            (
                PlatformCallFact::Returned | PlatformCallFact::Ambiguous,
                FinalStateFact::ExactApplied,
            ) => LocalExecutionOutcome::VerifiedApplied,
            (
                PlatformCallFact::NotEntered | PlatformCallFact::Returned,
                FinalStateFact::ExactNotApplied,
            ) => LocalExecutionOutcome::VerifiedNotApplied,
            (_, FinalStateFact::Insufficient)
                if evidence.call_fact == PlatformCallFact::Ambiguous =>
            {
                LocalExecutionOutcome::WriteOutcomeUnknown
            }
            (PlatformCallFact::NotEntered, FinalStateFact::ExactApplied)
            | (PlatformCallFact::Ambiguous, FinalStateFact::ExactNotApplied)
            | (_, FinalStateFact::Insufficient | FinalStateFact::Contradictory) => {
                LocalExecutionOutcome::NeedsReconcile
            }
        }
    };
    // The ledger stores a local authoritative fingerprint computed here, never
    // a caller-provided or R3 fingerprint. Timestamp is intentionally bound:
    // a retry must present exactly the same verifier record.
    let local_evidence_fingerprint = authoritative_local_evidence_fingerprint(
        &evidence,
        projection.contract_fingerprint,
        projection.collision_snapshot_fingerprint,
        outcome,
    );
    let outcome_id = authoritative_outcome_id(
        evidence.operation_id,
        evidence.attempt_number,
        evidence.boundary_id,
        evidence.boundary_occurred_at_unix_ms,
        evidence.evidence_id,
        local_evidence_fingerprint,
        outcome,
        evidence.recorded_at_unix_ms,
    );
    Ok(AuthoritativeFinalOutcome {
        outcome,
        operation_id: evidence.operation_id,
        attempt_number: evidence.attempt_number,
        intent_fingerprint: evidence.intent_fingerprint,
        contract_fingerprint: evidence.contract_fingerprint,
        collision_snapshot_fingerprint: evidence.collision_snapshot_fingerprint,
        boundary_id: evidence.boundary_id,
        boundary_occurred_at_unix_ms: evidence.boundary_occurred_at_unix_ms,
        evidence_id: evidence.evidence_id,
        r3_mutation_evidence_fingerprint: evidence.r3_mutation_evidence_fingerprint,
        evidence_fingerprint: local_evidence_fingerprint,
        outcome_id,
        recorded_at_unix_ms: evidence.recorded_at_unix_ms,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn authoritative_outcome_id(
    operation_id: Uuid,
    attempt_number: u32,
    boundary_id: Uuid,
    boundary_occurred_at_unix_ms: u64,
    evidence_id: Uuid,
    evidence_fingerprint: [u8; 32],
    outcome: LocalExecutionOutcome,
    recorded_at_unix_ms: u64,
) -> Uuid {
    let mut material =
        Vec::with_capacity(16 + 4 + 16 + 8 + 16 + 32 + 1 + 8 + OUTCOME_ID_DOMAIN.len());
    material.extend_from_slice(OUTCOME_ID_DOMAIN);
    material.extend_from_slice(operation_id.as_bytes());
    material.extend_from_slice(&attempt_number.to_be_bytes());
    material.extend_from_slice(boundary_id.as_bytes());
    material.extend_from_slice(&boundary_occurred_at_unix_ms.to_be_bytes());
    material.extend_from_slice(evidence_id.as_bytes());
    material.extend_from_slice(&evidence_fingerprint);
    material.push(outcome_tag(outcome));
    material.extend_from_slice(&recorded_at_unix_ms.to_be_bytes());
    let digest: [u8; 32] = Sha256::digest(material).into();
    Uuid::new_v5(&Uuid::NAMESPACE_OID, &digest)
}

fn authoritative_local_evidence_fingerprint(
    evidence: &AuthoritativeFinalEvidence,
    contract_fingerprint: [u8; 32],
    collision_snapshot_fingerprint: [u8; 32],
    outcome: LocalExecutionOutcome,
) -> [u8; 32] {
    let mut material =
        Vec::with_capacity(LOCAL_EVIDENCE_DOMAIN.len() + 16 + 4 + 16 + 8 + 32 * 4 + 16 + 8 + 8);
    material.extend_from_slice(LOCAL_EVIDENCE_DOMAIN);
    material.extend_from_slice(evidence.operation_id.as_bytes());
    material.extend_from_slice(&evidence.attempt_number.to_be_bytes());
    material.extend_from_slice(evidence.boundary_id.as_bytes());
    material.extend_from_slice(&evidence.boundary_occurred_at_unix_ms.to_be_bytes());
    material.extend_from_slice(&evidence.intent_fingerprint);
    material.extend_from_slice(&contract_fingerprint);
    material.extend_from_slice(&collision_snapshot_fingerprint);
    material.extend_from_slice(evidence.evidence_id.as_bytes());
    material.extend_from_slice(&evidence.recorded_at_unix_ms.to_be_bytes());
    material.push(evidence.call_fact as u8);
    material.push(evidence.verification as u8);
    material.push(evidence.side_effect as u8);
    material.push(evidence.final_state as u8);
    material.push(outcome_tag(outcome));
    // This binds the distinct R3 fact without treating it as the local proof.
    material.extend_from_slice(&evidence.r3_mutation_evidence_fingerprint);
    Sha256::digest(material).into()
}

const fn outcome_tag(outcome: LocalExecutionOutcome) -> u8 {
    match outcome {
        LocalExecutionOutcome::VerifiedApplied => 1,
        LocalExecutionOutcome::VerifiedNotApplied => 2,
        LocalExecutionOutcome::WriteOutcomeUnknown => 3,
        LocalExecutionOutcome::NeedsReconcile => 4,
    }
}

/// Redacted origin of a local inventory/revalidation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalExecutionEchoSource {
    DesktopWatcher,
    AndroidSaf,
}

/// Bounded, redacted notification that may request inventory only.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct LocalExecutionEchoHint {
    operation_id: Uuid,
    contract_fingerprint: [u8; 32],
    source: LocalExecutionEchoSource,
}

impl LocalExecutionEchoHint {
    /// Creates a hint with no paths, provider IDs, object IDs, or platform token.
    #[must_use]
    pub const fn new(
        operation_id: Uuid,
        contract_fingerprint: [u8; 32],
        source: LocalExecutionEchoSource,
    ) -> Self {
        Self {
            operation_id,
            contract_fingerprint,
            source,
        }
    }
}

impl fmt::Debug for LocalExecutionEchoHint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalExecutionEchoHint")
            .field("operation_id", &self.operation_id)
            .field("source", &self.source)
            .field("contract_fingerprint", &"<redacted>")
            .finish()
    }
}

/// Non-authoritative result of handling an echo hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EchoHintDisposition {
    InventoryRequired,
    DurableClaimPresentInventoryRequired,
}

/// Handles a watcher/SAF echo without writing journal, ledger, contract, or
/// cursor state.  It never returns an execution outcome.
///
/// # Errors
/// Returns a redacted mismatch error if the hint is not for this exact fresh
/// binding or if existing journal/ledger evidence is inconsistent.
pub fn handle_local_execution_echo_hint(
    store: &SyncStore,
    binding: &DurableExecutionBinding,
    attempt_number: u32,
    hint: LocalExecutionEchoHint,
) -> Result<EchoHintDisposition> {
    let projection = binding.persistence_projection();
    if hint.operation_id != projection.operation_id
        || hint.contract_fingerprint != projection.contract_fingerprint
    {
        return Err(crate::Error::LocalExecutionJournalMismatch);
    }
    match store.inspect_local_execution_recovery(binding, attempt_number)? {
        LocalExecutionRecoveryObservation::OutcomeWitnessAndLedgerMatch { .. } => {
            Ok(EchoHintDisposition::DurableClaimPresentInventoryRequired)
        }
        LocalExecutionRecoveryObservation::BoundaryWithoutWitness
        | LocalExecutionRecoveryObservation::PreSideEffectWitnessOnly
        | LocalExecutionRecoveryObservation::OutcomeWitnessPendingLedger { .. } => {
            Ok(EchoHintDisposition::InventoryRequired)
        }
    }
}

#[cfg(test)]
pub(crate) fn test_authoritative_evidence(
    binding: &DurableExecutionBinding,
    boundary: &LocalExecutionAttemptBoundary,
    call_fact: PlatformCallFact,
    outcome: LocalExecutionOutcome,
) -> AuthoritativeFinalEvidence {
    let evidence_id = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!(
            "r3.5-test-evidence:{}:{}",
            binding.persistence_projection().operation_id,
            boundary.attempt_number
        )
        .as_bytes(),
    );
    test_authoritative_evidence_with_identity(
        binding,
        boundary,
        call_fact,
        outcome,
        evidence_id,
        Sha256::digest(evidence_id.as_bytes()).into(),
    )
}

#[cfg(test)]
pub(crate) fn test_authoritative_evidence_with_identity(
    binding: &DurableExecutionBinding,
    boundary: &LocalExecutionAttemptBoundary,
    call_fact: PlatformCallFact,
    outcome: LocalExecutionOutcome,
    evidence_id: Uuid,
    r3_mutation_evidence_fingerprint: [u8; 32],
) -> AuthoritativeFinalEvidence {
    let projection = binding.persistence_projection();
    let final_state = match outcome {
        LocalExecutionOutcome::VerifiedApplied => FinalStateFact::ExactApplied,
        LocalExecutionOutcome::VerifiedNotApplied => FinalStateFact::ExactNotApplied,
        LocalExecutionOutcome::WriteOutcomeUnknown | LocalExecutionOutcome::NeedsReconcile => {
            FinalStateFact::Insufficient
        }
    };
    AuthoritativeFinalEvidence {
        operation_id: projection.operation_id,
        attempt_number: boundary.attempt_number,
        intent_fingerprint: projection.intent_fingerprint,
        contract_fingerprint: projection.contract_fingerprint,
        collision_snapshot_fingerprint: projection.collision_snapshot_fingerprint,
        boundary_id: boundary.boundary_id,
        boundary_occurred_at_unix_ms: boundary.occurred_at_unix_ms,
        evidence_id,
        r3_mutation_evidence_fingerprint,
        recorded_at_unix_ms: boundary.occurred_at_unix_ms + 1,
        call_fact,
        verification: BindingVerificationFact::Exact,
        side_effect: SideEffectFact::None,
        final_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_identity::test_durable_execution_binding;

    fn binding_and_boundary() -> (DurableExecutionBinding, LocalExecutionAttemptBoundary) {
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, Uuid::new_v4());
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 7,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 20,
        };
        (binding, boundary)
    }

    #[test]
    fn authoritative_final_outcome_matrix_is_fail_closed() {
        let (binding, boundary) = binding_and_boundary();
        for (requested, call_fact, expected) in [
            (
                LocalExecutionOutcome::VerifiedApplied,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
            ),
            (
                LocalExecutionOutcome::VerifiedNotApplied,
                PlatformCallFact::NotEntered,
                LocalExecutionOutcome::VerifiedNotApplied,
            ),
            (
                LocalExecutionOutcome::WriteOutcomeUnknown,
                PlatformCallFact::Ambiguous,
                LocalExecutionOutcome::WriteOutcomeUnknown,
            ),
            (
                LocalExecutionOutcome::NeedsReconcile,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::NeedsReconcile,
            ),
        ] {
            assert_eq!(
                classify_authoritative_final_outcome(
                    &binding,
                    &boundary,
                    test_authoritative_evidence(&binding, &boundary, call_fact, requested),
                )
                .expect("test evidence")
                .outcome(),
                expected
            );
        }
    }

    #[test]
    fn exhaustive_call_verification_side_effect_final_state_matrix_fails_closed() {
        let (binding, boundary) = binding_and_boundary();
        for call_fact in [
            PlatformCallFact::NotEntered,
            PlatformCallFact::Returned,
            PlatformCallFact::Ambiguous,
        ] {
            for verification in [
                BindingVerificationFact::Exact,
                BindingVerificationFact::Unsupported,
                BindingVerificationFact::BindingMismatch,
                BindingVerificationFact::CollisionMismatch,
                BindingVerificationFact::Substituted,
                BindingVerificationFact::Inconsistent,
            ] {
                for side_effect in [SideEffectFact::None, SideEffectFact::Forbidden] {
                    for final_state in [
                        FinalStateFact::ExactApplied,
                        FinalStateFact::ExactNotApplied,
                        FinalStateFact::Insufficient,
                        FinalStateFact::Contradictory,
                    ] {
                        let mut evidence = test_authoritative_evidence(
                            &binding,
                            &boundary,
                            call_fact,
                            LocalExecutionOutcome::NeedsReconcile,
                        );
                        evidence.verification = verification;
                        evidence.side_effect = side_effect;
                        evidence.final_state = final_state;
                        let actual =
                            classify_authoritative_final_outcome(&binding, &boundary, evidence)
                                .expect("test evidence")
                                .outcome();
                        let expected = if verification != BindingVerificationFact::Exact
                            || side_effect != SideEffectFact::None
                        {
                            LocalExecutionOutcome::NeedsReconcile
                        } else {
                            match (call_fact, final_state) {
                                (
                                    PlatformCallFact::Returned | PlatformCallFact::Ambiguous,
                                    FinalStateFact::ExactApplied,
                                ) => LocalExecutionOutcome::VerifiedApplied,
                                (
                                    PlatformCallFact::NotEntered | PlatformCallFact::Returned,
                                    FinalStateFact::ExactNotApplied,
                                ) => LocalExecutionOutcome::VerifiedNotApplied,
                                (PlatformCallFact::Ambiguous, FinalStateFact::Insufficient) => {
                                    LocalExecutionOutcome::WriteOutcomeUnknown
                                }
                                _ => LocalExecutionOutcome::NeedsReconcile,
                            }
                        };
                        assert_eq!(
                            actual, expected,
                            "{call_fact:?} {verification:?} {side_effect:?} {final_state:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unsupported_inconsistent_and_substituted_evidence_cannot_be_final() {
        let (binding, boundary) = binding_and_boundary();
        for verification in [
            BindingVerificationFact::Unsupported,
            BindingVerificationFact::BindingMismatch,
            BindingVerificationFact::CollisionMismatch,
            BindingVerificationFact::Substituted,
            BindingVerificationFact::Inconsistent,
        ] {
            let mut evidence = test_authoritative_evidence(
                &binding,
                &boundary,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
            );
            evidence.verification = verification;
            assert_eq!(
                classify_authoritative_final_outcome(&binding, &boundary, evidence)
                    .expect("test evidence")
                    .outcome(),
                LocalExecutionOutcome::NeedsReconcile
            );
        }
        let mut forbidden = test_authoritative_evidence(
            &binding,
            &boundary,
            PlatformCallFact::Returned,
            LocalExecutionOutcome::VerifiedApplied,
        );
        forbidden.side_effect = SideEffectFact::Forbidden;
        assert_eq!(
            classify_authoritative_final_outcome(&binding, &boundary, forbidden)
                .expect("test evidence")
                .outcome(),
            LocalExecutionOutcome::NeedsReconcile
        );
        let mut contradictory = test_authoritative_evidence(
            &binding,
            &boundary,
            PlatformCallFact::Returned,
            LocalExecutionOutcome::VerifiedApplied,
        );
        contradictory.final_state = FinalStateFact::Contradictory;
        assert_eq!(
            classify_authoritative_final_outcome(&binding, &boundary, contradictory)
                .expect("test evidence")
                .outcome(),
            LocalExecutionOutcome::NeedsReconcile
        );
    }

    #[test]
    fn boundary_id_and_timestamp_are_exact_hash_inputs_and_bindings() {
        let (binding, boundary) = binding_and_boundary();
        let evidence = test_authoritative_evidence(
            &binding,
            &boundary,
            PlatformCallFact::Returned,
            LocalExecutionOutcome::VerifiedApplied,
        );
        let exact = classify_authoritative_final_outcome(&binding, &boundary, evidence)
            .expect("exact evidence");
        let mut substituted_id = evidence;
        substituted_id.boundary_id = Uuid::new_v4();
        assert_eq!(
            classify_authoritative_final_outcome(&binding, &boundary, substituted_id)
                .expect("substituted evidence")
                .outcome(),
            LocalExecutionOutcome::NeedsReconcile
        );
        let mut substituted_time = evidence;
        substituted_time.boundary_occurred_at_unix_ms += 1;
        assert_eq!(
            classify_authoritative_final_outcome(&binding, &boundary, substituted_time)
                .expect("substituted evidence")
                .outcome(),
            LocalExecutionOutcome::NeedsReconcile
        );
        assert!(exact.matches_binding(&binding, &boundary));
        let other_boundary = LocalExecutionAttemptBoundary {
            boundary_id: Uuid::new_v4(),
            ..boundary
        };
        assert!(!exact.matches_binding(&binding, &other_boundary));
    }

    #[test]
    fn classifier_rejects_exact_identifier_and_timestamp_boundaries() {
        let (binding, boundary) = binding_and_boundary();
        let evidence = test_authoritative_evidence(
            &binding,
            &boundary,
            PlatformCallFact::Returned,
            LocalExecutionOutcome::VerifiedApplied,
        );
        for mutate in [
            |value: &mut AuthoritativeFinalEvidence| value.operation_id = Uuid::nil(),
            |value: &mut AuthoritativeFinalEvidence| value.evidence_id = Uuid::nil(),
            |value: &mut AuthoritativeFinalEvidence| value.boundary_id = Uuid::nil(),
        ] {
            let mut malformed = evidence;
            mutate(&mut malformed);
            assert!(matches!(
                classify_authoritative_final_outcome(&binding, &boundary, malformed),
                Err(crate::Error::InvalidLocalExecutionEvidence)
            ));
        }
        for malformed_boundary in [
            LocalExecutionAttemptBoundary {
                operation_id: Uuid::nil(),
                ..boundary
            },
            LocalExecutionAttemptBoundary {
                boundary_id: Uuid::nil(),
                ..boundary
            },
        ] {
            assert!(matches!(
                classify_authoritative_final_outcome(&binding, &malformed_boundary, evidence),
                Err(crate::Error::InvalidLocalExecutionEvidence)
            ));
        }
        for mutate in [
            |value: &mut AuthoritativeFinalEvidence| {
                value.recorded_at_unix_ms = i64::MAX as u64 + 1;
            },
            |value: &mut AuthoritativeFinalEvidence| {
                value.boundary_occurred_at_unix_ms = i64::MAX as u64 + 1;
            },
        ] {
            let mut oversized = evidence;
            mutate(&mut oversized);
            assert!(matches!(
                classify_authoritative_final_outcome(&binding, &boundary, oversized),
                Err(crate::Error::InvalidTimestamp)
            ));
        }
        let oversized_boundary = LocalExecutionAttemptBoundary {
            occurred_at_unix_ms: i64::MAX as u64 + 1,
            ..boundary
        };
        assert!(matches!(
            classify_authoritative_final_outcome(&binding, &oversized_boundary, evidence),
            Err(crate::Error::InvalidTimestamp)
        ));
    }

    #[test]
    fn matching_changed_boundary_rehashes_evidence_and_outcome_identifier() {
        let (binding, boundary) = binding_and_boundary();
        let original = classify_authoritative_final_outcome(
            &binding,
            &boundary,
            test_authoritative_evidence(
                &binding,
                &boundary,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
            ),
        )
        .expect("original classification");
        for changed_boundary in [
            LocalExecutionAttemptBoundary {
                boundary_id: Uuid::new_v4(),
                ..boundary
            },
            LocalExecutionAttemptBoundary {
                occurred_at_unix_ms: boundary.occurred_at_unix_ms + 1,
                ..boundary
            },
        ] {
            let changed = classify_authoritative_final_outcome(
                &binding,
                &changed_boundary,
                test_authoritative_evidence(
                    &binding,
                    &changed_boundary,
                    PlatformCallFact::Returned,
                    LocalExecutionOutcome::VerifiedApplied,
                ),
            )
            .expect("changed exact classification");
            assert_ne!(
                changed.evidence_fingerprint(),
                original.evidence_fingerprint()
            );
            assert_ne!(changed.outcome_id(), original.outcome_id());
        }
    }

    #[test]
    fn debug_is_redacted() {
        let (binding, boundary) = binding_and_boundary();
        let evidence = test_authoritative_evidence(
            &binding,
            &boundary,
            PlatformCallFact::Returned,
            LocalExecutionOutcome::VerifiedApplied,
        );
        let debug = format!("{evidence:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("test-provider"));
    }
}
