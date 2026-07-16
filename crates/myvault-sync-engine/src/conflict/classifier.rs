//! Pure R3.2 conflict classification for the frozen C01-C34 matrix.
//!
//! This module deliberately produces descriptions of work. It does not read bytes,
//! inspect a filesystem, allocate operation identifiers, persist evidence, or perform
//! local/provider mutations.

use super::{
    identity::is_exact_content_path, merge_markdown_three_way, normalized_collision_key,
    ContentFingerprint, MarkdownMergeIssue, MarkdownMergeOutcome,
};
use crate::{
    MutationDisposition, MutationEvidenceCapturePhase, MutationIntent, MutationOperationKind,
    MutationPhase, MutationState, MutationVerificationEvidence,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Canonical R3 conflict-matrix cell selected by [`classify_conflict`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictCell {
    C01,
    C02,
    C03,
    C04,
    C05,
    C06a,
    C06b,
    C07,
    C08,
    C09,
    C10,
    C11,
    C12,
    C13,
    C14a,
    C14b,
    C14c,
    C15,
    C16,
    C17,
    C18,
    C19,
    C20,
    C21,
    C22a,
    C22b,
    C23,
    C24,
    C25,
    C26,
    C27,
    C28a,
    C28b,
    C29,
    C30,
    C31,
    C32,
    C33a,
    C33b,
    C34,
}

impl ConflictCell {
    /// Stable, persistence-safe spelling used by the canonical contract.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::C01 => "C01",
            Self::C02 => "C02",
            Self::C03 => "C03",
            Self::C04 => "C04",
            Self::C05 => "C05",
            Self::C06a => "C06a",
            Self::C06b => "C06b",
            Self::C07 => "C07",
            Self::C08 => "C08",
            Self::C09 => "C09",
            Self::C10 => "C10",
            Self::C11 => "C11",
            Self::C12 => "C12",
            Self::C13 => "C13",
            Self::C14a => "C14a",
            Self::C14b => "C14b",
            Self::C14c => "C14c",
            Self::C15 => "C15",
            Self::C16 => "C16",
            Self::C17 => "C17",
            Self::C18 => "C18",
            Self::C19 => "C19",
            Self::C20 => "C20",
            Self::C21 => "C21",
            Self::C22a => "C22a",
            Self::C22b => "C22b",
            Self::C23 => "C23",
            Self::C24 => "C24",
            Self::C25 => "C25",
            Self::C26 => "C26",
            Self::C27 => "C27",
            Self::C28a => "C28a",
            Self::C28b => "C28b",
            Self::C29 => "C29",
            Self::C30 => "C30",
            Self::C31 => "C31",
            Self::C32 => "C32",
            Self::C33a => "C33a",
            Self::C33b => "C33b",
            Self::C34 => "C34",
        }
    }
}

/// The sole top-level result vocabulary allowed by the R3 contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictOutcome {
    NoOpVerified,
    GuardedLocalReplace,
    SafeTextMergeLocal,
    PreserveBothLocal,
    RemoteMutationBlocked,
    UnsupportedProtected,
    NeedsReconcile,
}

impl ConflictOutcome {
    /// Stable R3.1 redacted-code spelling for durable conflict evidence.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoOpVerified => "no_op_verified",
            Self::GuardedLocalReplace => "guarded_local_replace",
            Self::SafeTextMergeLocal => "safe_text_merge_local",
            Self::PreserveBothLocal => "preserve_both_local",
            Self::RemoteMutationBlocked => "remote_mutation_blocked",
            Self::UnsupportedProtected => "unsupported_protected",
            Self::NeedsReconcile => "needs_reconcile",
        }
    }
}

/// Whether the consumer can prove guarded, atomic no-replace local publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardedLocalCapability {
    Available,
    Unavailable,
    Unknown,
}

impl GuardedLocalCapability {
    const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Provider-state evidence needed for the double-delete C11 terminal result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeletedFinalStateProof {
    /// Exact identity and final provider state were verified deleted/trashed.
    Verified,
    /// The worker is offline or otherwise lacks exact final-state proof.
    NotVerified,
}

/// Exact identity/lineage/destination proof governing C13's optional rename draft.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalRenameGuard {
    ExactIdentityLineageAndNoCollision,
    NotProven,
}

/// Safety boundary evaluated before replay and conflict content facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryAssessment {
    Approved,
    ProtectedPath,
    UnsupportedObjectOrTopology,
    MalformedObjectMetadata,
    AccountRootOrAllowlistMismatch,
    AllowlistedIdentityLineageRevisionOrBaseMismatch,
}

/// Allowlisted retry description for a prior `VerifiedNotApplied` result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifiedNotAppliedRetry {
    GuardedLocalReplace,
    GuardedConflictCopy,
    RemoteExistingMutationBlocked,
    PreconditionsChangedOrCapabilityUnavailable,
}

/// Prior outcome assessment, evaluated after the safety boundary and before new facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayAssessment {
    Fresh,
    VerifiedAppliedExactPostState(ReplayProof),
    VerifiedNotApplied {
        proof: ReplayProof,
        retry: VerifiedNotAppliedRetry,
    },
    SideEffectOutcomeUnknown(ReplayProof),
    QueuedIntentCapturedBaseChanged(ReplayProof),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayDisposition {
    VerifiedAppliedExactPostState,
    VerifiedNotApplied,
    SideEffectOutcomeUnknown,
    QueuedIntentCapturedBaseChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayProof {
    operation_id: Uuid,
    state_version: u64,
    outcome_fingerprint: String,
    disposition: ReplayDisposition,
    operation_kind: MutationOperationKind,
    intent_fingerprint: String,
    account_id: Option<String>,
    remote_root_id: Option<String>,
    object_identity: Option<String>,
    identity_path: Option<String>,
    target_parent_id: Option<String>,
    base_sha256: Option<String>,
    base_byte_length: Option<u64>,
    expected_local_content: Option<ContentFingerprint>,
    expected_remote_content: Option<ContentFingerprint>,
    local_revision: Option<String>,
    remote_revision: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayProofFailure {
    InvalidIntent,
    InconsistentDurableOutcome,
    UnsupportedDisposition,
    BaseDidNotChange,
}

impl ReplayProof {
    /// Copies this sealed record's immutable binding keys into classification facts.
    pub fn bind_facts(&self, facts: &mut ClassificationEvidenceFacts) {
        facts.prior_operation_id = Some(self.operation_id);
        facts.prior_outcome_fingerprint = Some(self.outcome_fingerprint.clone());
        facts.prior_intent_fingerprint = Some(self.intent_fingerprint.clone());
        facts.prior_replay_disposition = Some(self.disposition);
    }

    #[must_use]
    pub const fn disposition(&self) -> ReplayDisposition {
        self.disposition
    }

    #[must_use]
    pub const fn expected_local_content(&self) -> Option<&ContentFingerprint> {
        self.expected_local_content.as_ref()
    }
    /// Seals replay evidence from the canonical R3.1 intent/state/evidence records.
    ///
    /// # Errors
    /// Rejects forged fingerprints, cross-operation records, invalid phases, or unsupported retry
    /// dispositions.
    pub fn from_r3_1(
        intent: &MutationIntent,
        state: &MutationState,
        evidence: &MutationVerificationEvidence,
    ) -> Result<Self, ReplayProofFailure> {
        if intent.operation_id != state.operation_id
            || intent.operation_id != evidence.operation_id
            || intent.intent_fingerprint != intent.canonical_fingerprint()
        {
            return Err(ReplayProofFailure::InvalidIntent);
        }
        let expected_local_content = intent_content_fingerprint(
            intent.expected_local_sha256.as_ref(),
            intent.expected_local_byte_length,
        )?;
        let expected_remote_content = intent_content_fingerprint(
            intent.expected_remote_sha256.as_ref(),
            intent.expected_remote_byte_length,
        )?;
        if evidence.evidence_fingerprint != evidence.canonical_fingerprint()
            || state.state_version == 0
            || state.last_evidence_id != Some(evidence.evidence_id)
            || state.disposition != Some(evidence.disposition)
            || state.outcome_code != evidence.outcome_code
        {
            return Err(ReplayProofFailure::InconsistentDurableOutcome);
        }
        let disposition = match evidence.disposition {
            MutationDisposition::VerifiedApplied
                if state.phase == MutationPhase::Completed
                    && evidence.capture_phase == MutationEvidenceCapturePhase::PostVerify
                    && post_state_matches_intent(intent, evidence) =>
            {
                ReplayDisposition::VerifiedAppliedExactPostState
            }
            MutationDisposition::VerifiedNotApplied
                if state.phase == MutationPhase::NeedsReconcile
                    && matches!(
                        evidence.capture_phase,
                        MutationEvidenceCapturePhase::PostVerify
                            | MutationEvidenceCapturePhase::Reconcile
                    ) =>
            {
                ReplayDisposition::VerifiedNotApplied
            }
            MutationDisposition::NeedsReconcile
                if state.phase == MutationPhase::NeedsReconcile
                    && evidence.capture_phase == MutationEvidenceCapturePhase::Reconcile =>
            {
                ReplayDisposition::SideEffectOutcomeUnknown
            }
            _ => return Err(ReplayProofFailure::UnsupportedDisposition),
        };
        Ok(Self {
            operation_id: intent.operation_id,
            state_version: state.state_version,
            outcome_fingerprint: evidence.evidence_fingerprint.clone(),
            disposition,
            operation_kind: intent.operation_kind,
            intent_fingerprint: intent.intent_fingerprint.clone(),
            account_id: intent.account_id.clone(),
            remote_root_id: intent.remote_root_id.clone(),
            object_identity: intent
                .remote_file_id
                .clone()
                .or_else(|| intent.local_object_id.clone()),
            identity_path: intent
                .source_path
                .clone()
                .or_else(|| intent.destination_path.clone()),
            target_parent_id: intent
                .destination_parent_id
                .clone()
                .or_else(|| intent.source_parent_id.clone()),
            base_sha256: intent.base_sha256.clone(),
            base_byte_length: intent.base_byte_length,
            expected_local_content,
            expected_remote_content,
            local_revision: intent.expected_local_revision.clone(),
            remote_revision: intent.expected_remote_revision.clone(),
        })
    }

    /// Seals an offline queued intent only after its captured base/revision tuple is proven stale.
    ///
    /// # Errors
    /// Rejects a forged intent, non-older state, or an unchanged captured tuple.
    pub fn from_queued_base_change(
        intent: &MutationIntent,
        captured_state_version: u64,
        current: &ClassificationEvidenceFacts,
    ) -> Result<Self, ReplayProofFailure> {
        if intent.intent_fingerprint != intent.canonical_fingerprint() {
            return Err(ReplayProofFailure::InvalidIntent);
        }
        let expected_local_content = intent_content_fingerprint(
            intent.expected_local_sha256.as_ref(),
            intent.expected_local_byte_length,
        )?;
        let expected_remote_content = intent_content_fingerprint(
            intent.expected_remote_sha256.as_ref(),
            intent.expected_remote_byte_length,
        )?;
        if captured_state_version == 0 || captured_state_version >= current.durable_state_version {
            return Err(ReplayProofFailure::BaseDidNotChange);
        }
        let current_base_sha = current.base.as_ref().map(|content| content.sha256.as_str());
        let changed = intent.base_sha256.as_deref() != current_base_sha
            || intent.expected_local_revision.as_deref() != current.local_revision.as_deref()
            || intent.expected_remote_revision.as_deref() != current.remote_revision.as_deref();
        if !changed {
            return Err(ReplayProofFailure::BaseDidNotChange);
        }
        Ok(Self {
            operation_id: intent.operation_id,
            state_version: captured_state_version,
            outcome_fingerprint: intent.intent_fingerprint.clone(),
            disposition: ReplayDisposition::QueuedIntentCapturedBaseChanged,
            operation_kind: intent.operation_kind,
            intent_fingerprint: intent.intent_fingerprint.clone(),
            account_id: intent.account_id.clone(),
            remote_root_id: intent.remote_root_id.clone(),
            object_identity: intent
                .remote_file_id
                .clone()
                .or_else(|| intent.local_object_id.clone()),
            identity_path: intent
                .source_path
                .clone()
                .or_else(|| intent.destination_path.clone()),
            target_parent_id: intent
                .destination_parent_id
                .clone()
                .or_else(|| intent.source_parent_id.clone()),
            base_sha256: intent.base_sha256.clone(),
            base_byte_length: intent.base_byte_length,
            expected_local_content,
            expected_remote_content,
            local_revision: intent.expected_local_revision.clone(),
            remote_revision: intent.expected_remote_revision.clone(),
        })
    }
}

fn intent_content_fingerprint(
    sha256: Option<&String>,
    byte_length: Option<u64>,
) -> Result<Option<ContentFingerprint>, ReplayProofFailure> {
    match (sha256, byte_length) {
        (None, None) => Ok(None),
        (Some(sha256), Some(byte_length)) if is_lower_hex_64(sha256) => {
            Ok(Some(ContentFingerprint {
                sha256: sha256.clone(),
                byte_length,
            }))
        }
        _ => Err(ReplayProofFailure::InvalidIntent),
    }
}

fn post_state_matches_intent(
    intent: &MutationIntent,
    evidence: &MutationVerificationEvidence,
) -> bool {
    evidence.observed_operation_marker.as_deref() == Some(intent.operation_marker.as_str())
        && intent
            .account_id
            .as_ref()
            .is_none_or(|value| evidence.observed_account_id.as_ref() == Some(value))
        && intent
            .remote_root_id
            .as_ref()
            .is_none_or(|value| evidence.observed_remote_root_id.as_ref() == Some(value))
        && intent
            .remote_file_id
            .as_ref()
            .is_none_or(|value| evidence.observed_remote_file_id.as_ref() == Some(value))
        && intent
            .destination_parent_id
            .as_ref()
            .is_none_or(|value| evidence.observed_parent_id.as_ref() == Some(value))
        && intent
            .destination_path
            .as_ref()
            .is_none_or(|value| evidence.observed_path.as_ref() == Some(value))
        && intent
            .expected_local_revision
            .as_ref()
            .is_none_or(|value| evidence.observed_local_revision.as_ref() == Some(value))
        && intent
            .expected_remote_revision
            .as_ref()
            .is_none_or(|value| evidence.observed_remote_revision.as_ref() == Some(value))
        && intent
            .expected_local_sha256
            .as_ref()
            .is_none_or(|value| evidence.observed_sha256.as_ref() == Some(value))
        && intent
            .expected_local_byte_length
            .is_none_or(|value| evidence.observed_byte_length == Some(value))
}

/// Complete precedence-bearing classifier input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictInput {
    evidence: ClassificationEvidence,
    boundary: BoundaryAssessment,
    replay: ReplayAssessment,
    case: ConflictCase,
}

impl ConflictInput {
    /// Builds a precedence-bearing input after validating immutable correctness evidence.
    ///
    /// # Errors
    /// Rejects incomplete approved identity/path/state evidence or content facts that do not
    /// contain the required base/local/remote fingerprints.
    pub fn new(
        evidence: ClassificationEvidence,
        boundary: BoundaryAssessment,
        replay: ReplayAssessment,
        case: ConflictCase,
    ) -> Result<Self, ConflictInputFailure> {
        validate_classification_evidence(&evidence, boundary, &replay, case)?;
        Ok(Self {
            evidence,
            boundary,
            replay,
            case,
        })
    }

    /// Builds a fresh approved input from exact immutable evidence.
    ///
    /// # Errors
    /// Rejects incomplete identity, state, path, or case-specific content evidence.
    pub fn fresh(
        evidence: ClassificationEvidence,
        case: ConflictCase,
    ) -> Result<Self, ConflictInputFailure> {
        Self::new(
            evidence,
            BoundaryAssessment::Approved,
            ReplayAssessment::Fresh,
            case,
        )
    }

    #[must_use]
    pub const fn evidence(&self) -> &ClassificationEvidence {
        &self.evidence
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassificationEvidence {
    facts: ClassificationEvidenceFacts,
    verified_markdown_merge: Option<ContentFingerprint>,
    verified_deleted_final_state: bool,
    verified_same_rename: Option<VerifiedSemanticTarget>,
    verified_same_move: Option<VerifiedSemanticTarget>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VerifiedSemanticTarget {
    account_id: String,
    remote_root_id: String,
    object_identity: String,
    durable_state_version: u64,
    target: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassificationEvidenceFacts {
    pub account_id: Option<String>,
    pub remote_root_id: Option<String>,
    pub object_identity: Option<String>,
    pub canonical_identity_path: Option<String>,
    pub target_parent_id: Option<String>,
    pub base: Option<ContentFingerprint>,
    pub local: Option<ContentFingerprint>,
    pub remote: Option<ContentFingerprint>,
    pub local_revision: Option<String>,
    pub remote_revision: Option<String>,
    pub durable_state_version: u64,
    pub prior_operation_id: Option<Uuid>,
    pub prior_outcome_fingerprint: Option<String>,
    pub prior_intent_fingerprint: Option<String>,
    pub prior_replay_disposition: Option<ReplayDisposition>,
    pub guarded_metadata_target: Option<GuardedMetadataTargetEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardedMetadataTargetEvidence {
    pub portable_path: String,
    pub parent_id: String,
    pub object_id: Option<String>,
    pub normalized_collision_key: String,
    pub occupied_collision_keys: Vec<String>,
}

impl ClassificationEvidence {
    #[must_use]
    pub const fn new(facts: ClassificationEvidenceFacts) -> Self {
        Self {
            facts,
            verified_markdown_merge: None,
            verified_deleted_final_state: false,
            verified_same_rename: None,
            verified_same_move: None,
        }
    }

    /// Runs the bounded Markdown engine and attaches its exact merged fingerprint as proof.
    ///
    /// # Errors
    /// Returns the engine's preserve-both reason when the input is unsafe or conflicting.
    pub fn verify_markdown_merge(
        mut self,
        base: &str,
        local: &str,
        remote: &str,
    ) -> Result<Self, MarkdownMergeIssue> {
        if self.facts.base.as_ref() != Some(&fingerprint_bytes(base.as_bytes()))
            || self.facts.local.as_ref() != Some(&fingerprint_bytes(local.as_bytes()))
            || self.facts.remote.as_ref() != Some(&fingerprint_bytes(remote.as_bytes()))
        {
            return Err(MarkdownMergeIssue::EvidenceFingerprintMismatch);
        }
        match merge_markdown_three_way(base, local, remote) {
            MarkdownMergeOutcome::Merged(merged) => {
                self.verified_markdown_merge = Some(ContentFingerprint {
                    sha256: merged.sha256,
                    byte_length: merged.byte_length,
                });
                Ok(self)
            }
            MarkdownMergeOutcome::PreserveBoth(issue) => Err(issue),
        }
    }

    #[must_use]
    pub const fn verified_markdown_merge(&self) -> Option<&ContentFingerprint> {
        self.verified_markdown_merge.as_ref()
    }

    #[must_use]
    pub const fn facts(&self) -> &ClassificationEvidenceFacts {
        &self.facts
    }

    /// Seals an exact local/provider deleted final-state observation against this identity.
    ///
    /// # Errors
    /// Rejects mismatched identity/state or a one-sided/non-final deletion observation.
    pub fn verify_deleted_final_state(
        mut self,
        account_id: &str,
        remote_root_id: &str,
        object_identity: &str,
        durable_state_version: u64,
        local_deleted: bool,
        remote_deleted: bool,
    ) -> Result<Self, ClassificationProofFailure> {
        if self.facts.account_id.as_deref() != Some(account_id)
            || self.facts.remote_root_id.as_deref() != Some(remote_root_id)
            || self.facts.object_identity.as_deref() != Some(object_identity)
            || self.facts.durable_state_version != durable_state_version
            || !local_deleted
            || !remote_deleted
        {
            return Err(ClassificationProofFailure::EvidenceMismatch);
        }
        self.verified_deleted_final_state = true;
        Ok(self)
    }

    /// Seals exact intended-name equality after portable path validation.
    ///
    /// # Errors
    /// Rejects unequal or invalid intended paths.
    pub fn verify_same_rename(
        mut self,
        account_id: &str,
        remote_root_id: &str,
        object_identity: &str,
        durable_state_version: u64,
        local_intended_path: &str,
        remote_intended_path: &str,
    ) -> Result<Self, ClassificationProofFailure> {
        if self.facts.account_id.as_deref() != Some(account_id)
            || self.facts.remote_root_id.as_deref() != Some(remote_root_id)
            || self.facts.object_identity.as_deref() != Some(object_identity)
            || self.facts.durable_state_version != durable_state_version
            || local_intended_path != remote_intended_path
            || !is_exact_content_path(local_intended_path)
        {
            return Err(ClassificationProofFailure::EvidenceMismatch);
        }
        self.verified_same_rename = Some(VerifiedSemanticTarget {
            account_id: account_id.to_owned(),
            remote_root_id: remote_root_id.to_owned(),
            object_identity: object_identity.to_owned(),
            durable_state_version,
            target: local_intended_path.to_owned(),
        });
        Ok(self)
    }

    /// Seals exact parent-lineage equality for a same-move result.
    ///
    /// # Errors
    /// Rejects unequal or invalid parent identities.
    pub fn verify_same_move(
        mut self,
        account_id: &str,
        remote_root_id: &str,
        object_identity: &str,
        durable_state_version: u64,
        local_parent_id: &str,
        remote_parent_id: &str,
    ) -> Result<Self, ClassificationProofFailure> {
        if self.facts.account_id.as_deref() != Some(account_id)
            || self.facts.remote_root_id.as_deref() != Some(remote_root_id)
            || self.facts.object_identity.as_deref() != Some(object_identity)
            || self.facts.durable_state_version != durable_state_version
            || local_parent_id != remote_parent_id
            || !is_remote_id(local_parent_id)
        {
            return Err(ClassificationProofFailure::EvidenceMismatch);
        }
        self.verified_same_move = Some(VerifiedSemanticTarget {
            account_id: account_id.to_owned(),
            remote_root_id: remote_root_id.to_owned(),
            object_identity: object_identity.to_owned(),
            durable_state_version,
            target: local_parent_id.to_owned(),
        });
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClassificationProofFailure {
    EvidenceMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictInputFailure {
    MissingExactIdentity,
    InvalidExactIdentity,
    InvalidContentEvidence,
    InvalidDurableStateVersion,
    MissingMarkdownMergeProof,
    InvalidReplayProof,
    MissingSemanticProof,
}

/// Semantic inputs for conflict facts after boundary and replay precedence pass.
///
/// Variant names describe facts rather than outcomes so policy remains centralized
/// in [`classify_conflict`]. Invalid combinations are unrepresentable: for example,
/// a caller cannot claim a non-overlapping text merge while also omitting its base.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictCase {
    /// Local unchanged; remote content changed from an exact base (C01).
    RemoteContentChanged {
        guarded_local_replace: GuardedLocalCapability,
    },
    /// Local content changed; remote remains at the exact base (C02).
    LocalContentChanged,
    /// Both regular Markdown versions changed, with a safe non-overlapping merge (C03).
    NonOverlappingMarkdownChanges,
    /// Both text versions changed and the frozen overlap rules report overlap (C04).
    OverlappingTextChanges,
    /// Both binary versions changed (C05).
    BothBinaryChanged,
    /// Both versions changed but the base is missing or ambiguous (C06a/C06b).
    BothChangedWithoutExactBase {
        guarded_no_replace: GuardedLocalCapability,
    },
    /// Changed text is invalid or ambiguously encoded (C07).
    InvalidOrAmbiguousText {
        guarded_no_replace: GuardedLocalCapability,
    },
    /// At least one merge input exceeds the frozen byte/line/combined bound (C08).
    MergeInputExceedsBounds {
        guarded_no_replace: GuardedLocalCapability,
    },
    /// Local delete/Trash intent conflicts with edited remote bytes (C09).
    LocalDeleteRemoteEdited,
    /// Edited local bytes conflict with remote delete/Trash state (C10).
    LocalEditedRemoteDelete,
    /// Both sides delete/Trash the exact identity (C11).
    BothDeleted { final_state: DeletedFinalStateProof },
    /// Local rename intent conflicts with remote edited content (C12).
    LocalRenameRemoteEdited,
    /// Edited local content conflicts with a remote rename (C13).
    LocalEditedRemoteRename { local_rename: LocalRenameGuard },
    /// Both sides renamed to the same exact intended-name bytes and identity (C14a).
    SameExactRename,
    /// Rename bytes differ but their portable Unicode collision key is equal (C14b).
    EquivalentButByteDifferentRenames,
    /// Both sides selected distinct, non-equivalent names (C14c).
    DivergentRenames,
    /// Local move intent conflicts with remote edited content (C15).
    LocalMoveRemoteEdited,
    /// Edited local content conflicts with a remote move (C16).
    LocalEditedRemoteMove { local_move: LocalRenameGuard },
    /// Both sides moved the exact identity to the same exact parent/lineage (C17).
    SameExactMove,
    /// Both sides moved the identity to different lineages (C18).
    DivergentMoves,
    /// A move/rename would create a parent or metadata cycle (fail-closed C18).
    RenameMoveCycle,
    /// A rename intent conflicts with a move intent (C19).
    RenameMoveConflict { combined_target: LocalRenameGuard },
    /// Local rename/move conflicts with remote delete/Trash (C20).
    LocalMetadataChangeRemoteDelete,
    /// Local delete/Trash conflicts with remote move/rename (C21).
    LocalDeleteRemoteMetadataChange,
    /// A locally-created destination blocks an arriving move/rename (C22a/C22b).
    DestinationCreatedVsMoveOrRename {
        guarded_no_replace: GuardedLocalCapability,
    },
    /// Distinct local and remote identities were created at equivalent paths (C23).
    DistinctCreatesAtEquivalentPath {
        guarded_no_replace: GuardedLocalCapability,
    },
    /// Different existing items move to the same target (C24).
    DifferentItemsMoveToSameTarget,
    /// More than one remote identity claims the same path (C25).
    DuplicateRemotePath,
    /// A changed child is observed with a moved/trashed parent (C26).
    ChildChangedWithParentChanged { resolved_lineage: LocalRenameGuard },
    /// A case/Unicode-equivalent destination collision (C34).
    PortableEquivalentDestinationCollision {
        guarded_no_replace: GuardedLocalCapability,
    },
}

fn validate_classification_evidence(
    evidence: &ClassificationEvidence,
    boundary: BoundaryAssessment,
    replay: &ReplayAssessment,
    case: ConflictCase,
) -> Result<(), ConflictInputFailure> {
    if boundary != BoundaryAssessment::Approved {
        return Ok(());
    }
    let facts = &evidence.facts;
    let exact_ids = [
        facts.account_id.as_deref(),
        facts.remote_root_id.as_deref(),
        facts.object_identity.as_deref(),
        facts.target_parent_id.as_deref(),
    ];
    if exact_ids.into_iter().any(|value| value.is_none()) {
        return Err(ConflictInputFailure::MissingExactIdentity);
    }
    if exact_ids
        .into_iter()
        .flatten()
        .any(|value| !is_remote_id(value))
        || !facts
            .canonical_identity_path
            .as_deref()
            .is_some_and(is_exact_content_path)
        || !facts.local_revision.as_deref().is_some_and(is_lower_hex_64)
        || !facts.remote_revision.as_deref().is_some_and(is_remote_id)
    {
        return Err(ConflictInputFailure::InvalidExactIdentity);
    }
    if facts.durable_state_version == 0 {
        return Err(ConflictInputFailure::InvalidDurableStateVersion);
    }
    if facts
        .guarded_metadata_target
        .as_ref()
        .is_some_and(|target| {
            !is_exact_content_path(&target.portable_path)
                || !is_remote_id(&target.parent_id)
                || target
                    .object_id
                    .as_deref()
                    .is_some_and(|id| !is_remote_id(id))
                || normalized_collision_key(&target.portable_path).as_deref()
                    != Ok(target.normalized_collision_key.as_str())
                || target.occupied_collision_keys.len() > 4_096
                || target
                    .occupied_collision_keys
                    .iter()
                    .any(|key| !is_lower_hex_64(key) || key == &target.normalized_collision_key)
        })
    {
        return Err(ConflictInputFailure::InvalidExactIdentity);
    }
    if case_requires_guarded_target(case) && facts.guarded_metadata_target.is_none() {
        return Err(ConflictInputFailure::MissingSemanticProof);
    }
    validate_replay_proof(evidence, replay)?;
    if [
        facts.base.as_ref(),
        facts.local.as_ref(),
        facts.remote.as_ref(),
        evidence.verified_markdown_merge.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|content| !is_lower_hex_64(&content.sha256) || i64::try_from(content.byte_length).is_err())
    {
        return Err(ConflictInputFailure::InvalidContentEvidence);
    }
    let required = required_content(case);
    if (required.base && facts.base.is_none())
        || (required.local && facts.local.is_none())
        || (required.remote && facts.remote.is_none())
    {
        return Err(ConflictInputFailure::InvalidContentEvidence);
    }
    if matches!(replay, ReplayAssessment::Fresh)
        && case == ConflictCase::NonOverlappingMarkdownChanges
        && evidence.verified_markdown_merge.is_none()
    {
        return Err(ConflictInputFailure::MissingMarkdownMergeProof);
    }
    validate_content_predicates(facts, replay, case)?;
    if matches!(
        case,
        ConflictCase::BothDeleted {
            final_state: DeletedFinalStateProof::Verified
        }
    ) && !evidence.verified_deleted_final_state
        || case == ConflictCase::SameExactRename
            && !semantic_target_matches(evidence.verified_same_rename.as_ref(), facts)
        || case == ConflictCase::SameExactMove
            && !semantic_target_matches(evidence.verified_same_move.as_ref(), facts)
    {
        return Err(ConflictInputFailure::MissingSemanticProof);
    }
    Ok(())
}

fn semantic_target_matches(
    proof: Option<&VerifiedSemanticTarget>,
    facts: &ClassificationEvidenceFacts,
) -> bool {
    proof.is_some_and(|proof| {
        facts.account_id.as_deref() == Some(proof.account_id.as_str())
            && facts.remote_root_id.as_deref() == Some(proof.remote_root_id.as_str())
            && facts.object_identity.as_deref() == Some(proof.object_identity.as_str())
            && facts.durable_state_version == proof.durable_state_version
            && !proof.target.is_empty()
    })
}

fn validate_content_predicates(
    facts: &ClassificationEvidenceFacts,
    replay: &ReplayAssessment,
    case: ConflictCase,
) -> Result<(), ConflictInputFailure> {
    if !matches!(replay, ReplayAssessment::Fresh) {
        let replay_required = match replay {
            ReplayAssessment::VerifiedNotApplied { retry, .. } => match retry {
                VerifiedNotAppliedRetry::GuardedLocalReplace
                | VerifiedNotAppliedRetry::GuardedConflictCopy => RequiredContent {
                    base: false,
                    local: false,
                    remote: true,
                },
                VerifiedNotAppliedRetry::RemoteExistingMutationBlocked
                | VerifiedNotAppliedRetry::PreconditionsChangedOrCapabilityUnavailable => {
                    RequiredContent {
                        base: false,
                        local: false,
                        remote: false,
                    }
                }
            },
            _ => RequiredContent {
                base: false,
                local: false,
                remote: false,
            },
        };
        return require_content(facts, replay_required);
    }

    let valid = match case {
        ConflictCase::RemoteContentChanged { .. } => {
            facts.local == facts.base && facts.remote != facts.base
        }
        ConflictCase::LocalContentChanged => {
            facts.remote == facts.base && facts.local != facts.base
        }
        ConflictCase::NonOverlappingMarkdownChanges
        | ConflictCase::OverlappingTextChanges
        | ConflictCase::BothBinaryChanged => {
            facts.local != facts.base && facts.remote != facts.base
        }
        ConflictCase::BothChangedWithoutExactBase { .. } => {
            facts.base.is_none() && facts.local.is_some() && facts.remote.is_some()
        }
        ConflictCase::InvalidOrAmbiguousText { .. }
        | ConflictCase::MergeInputExceedsBounds { .. } => {
            facts.local.is_some()
                && facts.remote.is_some()
                && (facts.base.is_none() || facts.local != facts.base || facts.remote != facts.base)
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(ConflictInputFailure::InvalidContentEvidence)
    }
}

const fn case_requires_guarded_target(case: ConflictCase) -> bool {
    matches!(
        case,
        ConflictCase::LocalEditedRemoteRename {
            local_rename: LocalRenameGuard::ExactIdentityLineageAndNoCollision
        } | ConflictCase::LocalEditedRemoteMove {
            local_move: LocalRenameGuard::ExactIdentityLineageAndNoCollision
        } | ConflictCase::RenameMoveConflict {
            combined_target: LocalRenameGuard::ExactIdentityLineageAndNoCollision
        } | ConflictCase::ChildChangedWithParentChanged {
            resolved_lineage: LocalRenameGuard::ExactIdentityLineageAndNoCollision
        }
    )
}

fn require_content(
    facts: &ClassificationEvidenceFacts,
    required: RequiredContent,
) -> Result<(), ConflictInputFailure> {
    if (required.base && facts.base.is_none())
        || (required.local && facts.local.is_none())
        || (required.remote && facts.remote.is_none())
    {
        Err(ConflictInputFailure::InvalidContentEvidence)
    } else {
        Ok(())
    }
}

fn validate_replay_proof(
    evidence: &ClassificationEvidence,
    replay: &ReplayAssessment,
) -> Result<(), ConflictInputFailure> {
    let facts = evidence.facts();
    let (proof, expected_disposition) = match replay {
        ReplayAssessment::Fresh => {
            return if facts.prior_operation_id.is_none()
                && facts.prior_outcome_fingerprint.is_none()
                && facts.prior_intent_fingerprint.is_none()
                && facts.prior_replay_disposition.is_none()
            {
                Ok(())
            } else {
                Err(ConflictInputFailure::InvalidReplayProof)
            };
        }
        ReplayAssessment::VerifiedAppliedExactPostState(proof) => {
            (proof, ReplayDisposition::VerifiedAppliedExactPostState)
        }
        ReplayAssessment::VerifiedNotApplied { proof, .. } => {
            (proof, ReplayDisposition::VerifiedNotApplied)
        }
        ReplayAssessment::SideEffectOutcomeUnknown(proof) => {
            (proof, ReplayDisposition::SideEffectOutcomeUnknown)
        }
        ReplayAssessment::QueuedIntentCapturedBaseChanged(proof) => {
            (proof, ReplayDisposition::QueuedIntentCapturedBaseChanged)
        }
    };
    let version_matches = if matches!(replay, ReplayAssessment::QueuedIntentCapturedBaseChanged(_))
    {
        proof.state_version <= facts.durable_state_version
    } else {
        proof.state_version == facts.durable_state_version
    };
    if proof.operation_id.is_nil()
        || !version_matches
        || !is_lower_hex_64(&proof.outcome_fingerprint)
        || facts.prior_operation_id != Some(proof.operation_id)
        || facts.prior_outcome_fingerprint.as_deref() != Some(proof.outcome_fingerprint.as_str())
        || facts.prior_intent_fingerprint.as_deref() != Some(proof.intent_fingerprint.as_str())
        || proof.disposition != expected_disposition
        || facts.prior_replay_disposition != Some(expected_disposition)
        || !replay_operation_matches(replay, proof.operation_kind)
        || !replay_snapshot_matches(evidence, proof)
    {
        Err(ConflictInputFailure::InvalidReplayProof)
    } else {
        Ok(())
    }
}

fn replay_snapshot_matches(evidence: &ClassificationEvidence, proof: &ReplayProof) -> bool {
    let facts = evidence.facts();
    let identity_matches = proof.account_id.as_deref() == facts.account_id.as_deref()
        && proof.remote_root_id.as_deref() == facts.remote_root_id.as_deref()
        && proof.object_identity.as_deref() == facts.object_identity.as_deref()
        && proof.identity_path.as_deref() == facts.canonical_identity_path.as_deref()
        && proof.target_parent_id.as_deref() == facts.target_parent_id.as_deref();
    let prior_base_matches_current = proof.base_sha256.as_deref()
        == facts.base.as_ref().map(|content| content.sha256.as_str())
        && proof.base_byte_length == facts.base.as_ref().map(|content| content.byte_length);
    let base_matches = if proof.disposition == ReplayDisposition::VerifiedAppliedExactPostState
        && proof.operation_kind == MutationOperationKind::BasePublish
    {
        true
    } else {
        prior_base_matches_current
    };
    let payload_matches = match proof.operation_kind {
        MutationOperationKind::LocalPublish | MutationOperationKind::ConflictCopyPublish => {
            proof.expected_local_content.as_ref() == facts.remote.as_ref()
        }
        MutationOperationKind::MergePublish => proof.expected_local_content.is_some(),
        MutationOperationKind::BasePublish => {
            proof.expected_local_content.as_ref() == facts.base.as_ref()
        }
        MutationOperationKind::RemoteExistingBlocked => true,
    };
    let remote_matches = proof.expected_remote_content.is_none()
        || proof.expected_remote_content.as_ref() == facts.remote.as_ref();
    let tuple_matches = base_matches
        && proof.local_revision.as_deref() == facts.local_revision.as_deref()
        && proof.remote_revision.as_deref() == facts.remote_revision.as_deref()
        && payload_matches
        && remote_matches;
    let exact_post_state_matches = proof.disposition
        != ReplayDisposition::VerifiedAppliedExactPostState
        || match proof.operation_kind {
            MutationOperationKind::LocalPublish | MutationOperationKind::MergePublish => {
                facts.local.as_ref() == proof.expected_local_content.as_ref()
            }
            MutationOperationKind::ConflictCopyPublish
            | MutationOperationKind::RemoteExistingBlocked => true,
            MutationOperationKind::BasePublish => {
                facts.base.as_ref() == proof.expected_local_content.as_ref()
            }
        };
    identity_matches
        && if proof.disposition == ReplayDisposition::QueuedIntentCapturedBaseChanged {
            !tuple_matches
        } else {
            tuple_matches && exact_post_state_matches
        }
}

fn replay_operation_matches(replay: &ReplayAssessment, kind: MutationOperationKind) -> bool {
    match replay {
        ReplayAssessment::VerifiedNotApplied { retry, .. } => match retry {
            VerifiedNotAppliedRetry::GuardedLocalReplace => {
                kind == MutationOperationKind::LocalPublish
            }
            VerifiedNotAppliedRetry::GuardedConflictCopy => {
                kind == MutationOperationKind::ConflictCopyPublish
            }
            VerifiedNotAppliedRetry::RemoteExistingMutationBlocked => {
                kind == MutationOperationKind::RemoteExistingBlocked
            }
            VerifiedNotAppliedRetry::PreconditionsChangedOrCapabilityUnavailable => true,
        },
        _ => true,
    }
}

fn fingerprint_bytes(bytes: &[u8]) -> ContentFingerprint {
    let digest = Sha256::digest(bytes);
    ContentFingerprint {
        sha256: format!("{digest:x}"),
        byte_length: u64::try_from(bytes.len()).expect("bounded Markdown length fits u64"),
    }
}

#[derive(Clone, Copy)]
struct RequiredContent {
    base: bool,
    local: bool,
    remote: bool,
}

const fn required_content(case: ConflictCase) -> RequiredContent {
    match case {
        ConflictCase::RemoteContentChanged { .. }
        | ConflictCase::LocalContentChanged
        | ConflictCase::NonOverlappingMarkdownChanges
        | ConflictCase::OverlappingTextChanges
        | ConflictCase::BothBinaryChanged => RequiredContent {
            base: true,
            local: true,
            remote: true,
        },
        ConflictCase::BothChangedWithoutExactBase { .. }
        | ConflictCase::InvalidOrAmbiguousText { .. }
        | ConflictCase::MergeInputExceedsBounds { .. }
        | ConflictCase::DestinationCreatedVsMoveOrRename { .. }
        | ConflictCase::DistinctCreatesAtEquivalentPath { .. }
        | ConflictCase::PortableEquivalentDestinationCollision { .. } => RequiredContent {
            base: false,
            local: true,
            remote: true,
        },
        ConflictCase::LocalDeleteRemoteEdited
        | ConflictCase::LocalRenameRemoteEdited
        | ConflictCase::LocalMoveRemoteEdited => RequiredContent {
            base: false,
            local: false,
            remote: true,
        },
        ConflictCase::LocalEditedRemoteDelete | ConflictCase::LocalEditedRemoteRename { .. } => {
            RequiredContent {
                base: false,
                local: true,
                remote: false,
            }
        }
        _ => RequiredContent {
            base: false,
            local: false,
            remote: false,
        },
    }
}

fn is_remote_id(value: &str) -> bool {
    (1..=512).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Pure operation drafts. They are requirements, not authorization to execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictDraft {
    /// Publish verified remote bytes over the unchanged local object.
    LocalContentPublish,
    /// Publish the deterministic three-way merge locally.
    MergePublish,
    /// Publish a deterministic no-replace conflict copy locally.
    ConflictCopyPublish,
    /// Publish exact resolved bytes as the next immutable base.
    BasePublish,
    /// Retain a durable existing-item provider mutation intent without sending it.
    RemoteExistingBlocked,
    /// Apply a remote name locally only after the exact C13 guard is rechecked.
    GuardedLocalRename,
    /// Apply an exact remote parent lineage locally after rechecking the guard.
    GuardedLocalMove,
    /// Apply a combined name/parent target locally after rechecking all guards.
    GuardedLocalRenameMove,
}

/// User data or intent that the materializer must not discard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedEvidence {
    LocalBytes,
    RemoteBytes,
    DeleteIntent,
    LocalRenameIntent,
    RemoteRenameIntent,
    MoveIntent,
    LocalIdentity,
    RemoteIdentity,
    ParentLineage,
    DestinationIdentity,
    ProviderMetadata,
    PreserveBothEvidence,
}

/// What must be durably completed before a consumer may advance its cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorGate {
    /// Exact final-state verification is sufficient; there is no publication draft.
    ExactVerification,
    /// Every emitted local publication/base dependency needs completed evidence.
    AllLocalPublications,
    /// The result is deliberately terminal at reconciliation; advancement is forbidden.
    NeedsReconcile,
}

/// Immutable pure classifier result consumed by identity/materialization code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictPlan {
    pub(crate) cell: ConflictCell,
    pub(crate) outcome: ConflictOutcome,
    pub(crate) drafts: Vec<ConflictDraft>,
    pub(crate) retained: Vec<RetainedEvidence>,
    pub(crate) cursor_gate: CursorGate,
    classification_evidence: Option<ClassificationEvidence>,
}

impl ConflictPlan {
    fn new(
        cell: ConflictCell,
        outcome: ConflictOutcome,
        drafts: &[ConflictDraft],
        retained: &[RetainedEvidence],
        cursor_gate: CursorGate,
    ) -> Self {
        Self {
            cell,
            outcome,
            drafts: drafts.to_vec(),
            retained: retained.to_vec(),
            cursor_gate,
            classification_evidence: None,
        }
    }

    fn bind_evidence(mut self, evidence: ClassificationEvidence) -> Self {
        self.classification_evidence = Some(evidence);
        self
    }

    #[must_use]
    /// Returns the exact evidence captured with this public classification result.
    ///
    /// # Panics
    /// This invariant assertion can panic only if an internal classifier helper bypasses
    /// [`classify_conflict`]; callers cannot construct an unbound [`ConflictPlan`].
    pub fn evidence(&self) -> &ClassificationEvidence {
        self.classification_evidence
            .as_ref()
            .expect("public conflict plans always bind validated evidence")
    }

    #[must_use]
    pub const fn cell(&self) -> ConflictCell {
        self.cell
    }

    #[must_use]
    pub const fn outcome(&self) -> ConflictOutcome {
        self.outcome
    }

    #[must_use]
    pub fn drafts(&self) -> &[ConflictDraft] {
        &self.drafts
    }

    #[must_use]
    pub fn retained(&self) -> &[RetainedEvidence] {
        &self.retained
    }

    #[must_use]
    pub const fn cursor_gate(&self) -> CursorGate {
        self.cursor_gate
    }
}

/// Classify one canonical scenario with frozen boundary/replay precedence.
#[must_use]
pub fn classify_conflict(input: ConflictInput) -> ConflictPlan {
    let ConflictInput {
        evidence,
        boundary,
        replay,
        case,
    } = input;
    let plan = classify_boundary(boundary)
        .or_else(|| classify_replay(&replay))
        .unwrap_or_else(|| classify_fresh_case(case));
    plan.bind_evidence(evidence)
}

// Keeping the canonical matrix in one visibly exhaustive match makes policy review
// safer than scattering cells across helpers whose fall-through order could drift.
#[allow(clippy::too_many_lines)]
fn classify_fresh_case(case: ConflictCase) -> ConflictPlan {
    match case {
        ConflictCase::RemoteContentChanged {
            guarded_local_replace,
        } => {
            if guarded_local_replace.is_available() {
                ConflictPlan::new(
                    ConflictCell::C01,
                    ConflictOutcome::GuardedLocalReplace,
                    &[
                        ConflictDraft::LocalContentPublish,
                        ConflictDraft::BasePublish,
                    ],
                    &[RetainedEvidence::RemoteBytes],
                    CursorGate::AllLocalPublications,
                )
            } else {
                needs_reconcile(ConflictCell::C01, &[], &[RetainedEvidence::RemoteBytes])
            }
        }
        ConflictCase::LocalContentChanged => ConflictPlan::new(
            ConflictCell::C02,
            ConflictOutcome::RemoteMutationBlocked,
            &[ConflictDraft::RemoteExistingBlocked],
            &[RetainedEvidence::LocalBytes],
            CursorGate::NeedsReconcile,
        ),
        ConflictCase::NonOverlappingMarkdownChanges => ConflictPlan::new(
            ConflictCell::C03,
            ConflictOutcome::SafeTextMergeLocal,
            &[
                ConflictDraft::MergePublish,
                ConflictDraft::BasePublish,
                ConflictDraft::RemoteExistingBlocked,
            ],
            &[RetainedEvidence::LocalBytes, RetainedEvidence::RemoteBytes],
            CursorGate::NeedsReconcile,
        ),
        ConflictCase::OverlappingTextChanges => preserve_both(ConflictCell::C04),
        ConflictCase::BothBinaryChanged => preserve_both(ConflictCell::C05),
        ConflictCase::BothChangedWithoutExactBase { guarded_no_replace } => {
            preserve_if_available(ConflictCell::C06a, ConflictCell::C06b, guarded_no_replace)
        }
        ConflictCase::InvalidOrAmbiguousText { guarded_no_replace } => {
            preserve_or_reconcile(ConflictCell::C07, guarded_no_replace)
        }
        ConflictCase::MergeInputExceedsBounds { guarded_no_replace } => {
            preserve_or_reconcile(ConflictCell::C08, guarded_no_replace)
        }
        ConflictCase::LocalDeleteRemoteEdited => ConflictPlan::new(
            ConflictCell::C09,
            ConflictOutcome::NeedsReconcile,
            &[
                ConflictDraft::LocalContentPublish,
                ConflictDraft::BasePublish,
                ConflictDraft::RemoteExistingBlocked,
            ],
            &[
                RetainedEvidence::RemoteBytes,
                RetainedEvidence::DeleteIntent,
            ],
            CursorGate::NeedsReconcile,
        ),
        ConflictCase::LocalEditedRemoteDelete => needs_reconcile(
            ConflictCell::C10,
            &[ConflictDraft::RemoteExistingBlocked],
            &[
                RetainedEvidence::LocalBytes,
                RetainedEvidence::DeleteIntent,
                RetainedEvidence::PreserveBothEvidence,
            ],
        ),
        ConflictCase::BothDeleted { final_state } => match final_state {
            DeletedFinalStateProof::Verified => ConflictPlan::new(
                ConflictCell::C11,
                ConflictOutcome::NoOpVerified,
                &[],
                &[RetainedEvidence::DeleteIntent],
                CursorGate::ExactVerification,
            ),
            DeletedFinalStateProof::NotVerified => {
                needs_reconcile(ConflictCell::C11, &[], &[RetainedEvidence::DeleteIntent])
            }
        },
        ConflictCase::LocalRenameRemoteEdited => ConflictPlan::new(
            ConflictCell::C12,
            ConflictOutcome::RemoteMutationBlocked,
            &[
                ConflictDraft::LocalContentPublish,
                ConflictDraft::BasePublish,
                ConflictDraft::RemoteExistingBlocked,
            ],
            &[
                RetainedEvidence::RemoteBytes,
                RetainedEvidence::LocalRenameIntent,
            ],
            CursorGate::NeedsReconcile,
        ),
        ConflictCase::LocalEditedRemoteRename { local_rename } => {
            let drafts = match local_rename {
                LocalRenameGuard::ExactIdentityLineageAndNoCollision => {
                    &[ConflictDraft::GuardedLocalRename][..]
                }
                LocalRenameGuard::NotProven => &[],
            };
            needs_reconcile(
                ConflictCell::C13,
                drafts,
                &[
                    RetainedEvidence::LocalBytes,
                    RetainedEvidence::RemoteRenameIntent,
                ],
            )
        }
        ConflictCase::SameExactRename => ConflictPlan::new(
            ConflictCell::C14a,
            ConflictOutcome::NoOpVerified,
            &[],
            &[RetainedEvidence::LocalRenameIntent],
            CursorGate::ExactVerification,
        ),
        ConflictCase::EquivalentButByteDifferentRenames => needs_reconcile(
            ConflictCell::C14b,
            &[],
            &[
                RetainedEvidence::LocalRenameIntent,
                RetainedEvidence::RemoteRenameIntent,
            ],
        ),
        ConflictCase::DivergentRenames => needs_reconcile(
            ConflictCell::C14c,
            &[],
            &[
                RetainedEvidence::LocalRenameIntent,
                RetainedEvidence::RemoteRenameIntent,
            ],
        ),
        ConflictCase::LocalMoveRemoteEdited => needs_reconcile(
            ConflictCell::C15,
            &[
                ConflictDraft::LocalContentPublish,
                ConflictDraft::RemoteExistingBlocked,
            ],
            &[RetainedEvidence::RemoteBytes, RetainedEvidence::MoveIntent],
        ),
        ConflictCase::LocalEditedRemoteMove { local_move } => {
            let drafts = guarded_draft(local_move, ConflictDraft::GuardedLocalMove);
            needs_reconcile(
                ConflictCell::C16,
                drafts,
                &[RetainedEvidence::LocalBytes, RetainedEvidence::MoveIntent],
            )
        }
        ConflictCase::SameExactMove => ConflictPlan::new(
            ConflictCell::C17,
            ConflictOutcome::NoOpVerified,
            &[],
            &[RetainedEvidence::ParentLineage],
            CursorGate::ExactVerification,
        ),
        ConflictCase::DivergentMoves => needs_reconcile(
            ConflictCell::C18,
            &[],
            &[
                RetainedEvidence::MoveIntent,
                RetainedEvidence::ParentLineage,
            ],
        ),
        ConflictCase::RenameMoveCycle => needs_reconcile(
            ConflictCell::C18,
            &[],
            &[
                RetainedEvidence::MoveIntent,
                RetainedEvidence::LocalRenameIntent,
                RetainedEvidence::ParentLineage,
            ],
        ),
        ConflictCase::RenameMoveConflict { combined_target } => {
            let drafts = guarded_draft(combined_target, ConflictDraft::GuardedLocalRenameMove);
            needs_reconcile(
                ConflictCell::C19,
                drafts,
                &[
                    RetainedEvidence::LocalRenameIntent,
                    RetainedEvidence::MoveIntent,
                ],
            )
        }
        ConflictCase::LocalMetadataChangeRemoteDelete => needs_reconcile(
            ConflictCell::C20,
            &[ConflictDraft::RemoteExistingBlocked],
            &[
                RetainedEvidence::LocalRenameIntent,
                RetainedEvidence::MoveIntent,
                RetainedEvidence::DeleteIntent,
            ],
        ),
        ConflictCase::LocalDeleteRemoteMetadataChange => needs_reconcile(
            ConflictCell::C21,
            &[],
            &[
                RetainedEvidence::DeleteIntent,
                RetainedEvidence::RemoteRenameIntent,
                RetainedEvidence::MoveIntent,
                RetainedEvidence::RemoteIdentity,
            ],
        ),
        ConflictCase::DestinationCreatedVsMoveOrRename { guarded_no_replace } => {
            preserve_if_available(ConflictCell::C22a, ConflictCell::C22b, guarded_no_replace)
        }
        ConflictCase::DistinctCreatesAtEquivalentPath { guarded_no_replace } => {
            preserve_or_reconcile(ConflictCell::C23, guarded_no_replace)
        }
        ConflictCase::DifferentItemsMoveToSameTarget => needs_reconcile(
            ConflictCell::C24,
            &[],
            &[
                RetainedEvidence::LocalIdentity,
                RetainedEvidence::RemoteIdentity,
                RetainedEvidence::DestinationIdentity,
            ],
        ),
        ConflictCase::DuplicateRemotePath => needs_reconcile(
            ConflictCell::C25,
            &[],
            &[
                RetainedEvidence::RemoteIdentity,
                RetainedEvidence::DestinationIdentity,
            ],
        ),
        ConflictCase::ChildChangedWithParentChanged { resolved_lineage } => {
            let drafts = guarded_draft(resolved_lineage, ConflictDraft::GuardedLocalMove);
            needs_reconcile(
                ConflictCell::C26,
                drafts,
                &[
                    RetainedEvidence::LocalBytes,
                    RetainedEvidence::ParentLineage,
                ],
            )
        }
        ConflictCase::PortableEquivalentDestinationCollision { guarded_no_replace } => {
            preserve_or_reconcile(ConflictCell::C34, guarded_no_replace)
        }
    }
}

fn classify_boundary(boundary: BoundaryAssessment) -> Option<ConflictPlan> {
    let (cell, outcome, retained) = match boundary {
        BoundaryAssessment::Approved => return None,
        BoundaryAssessment::ProtectedPath => (
            ConflictCell::C27,
            ConflictOutcome::UnsupportedProtected,
            &[RetainedEvidence::DestinationIdentity][..],
        ),
        BoundaryAssessment::UnsupportedObjectOrTopology => (
            ConflictCell::C28a,
            ConflictOutcome::UnsupportedProtected,
            &[RetainedEvidence::ProviderMetadata][..],
        ),
        BoundaryAssessment::MalformedObjectMetadata => (
            ConflictCell::C28b,
            ConflictOutcome::NeedsReconcile,
            &[RetainedEvidence::ProviderMetadata][..],
        ),
        BoundaryAssessment::AccountRootOrAllowlistMismatch => (
            ConflictCell::C33a,
            ConflictOutcome::UnsupportedProtected,
            &[RetainedEvidence::ProviderMetadata][..],
        ),
        BoundaryAssessment::AllowlistedIdentityLineageRevisionOrBaseMismatch => (
            ConflictCell::C33b,
            ConflictOutcome::NeedsReconcile,
            &[
                RetainedEvidence::ProviderMetadata,
                RetainedEvidence::ParentLineage,
            ][..],
        ),
    };
    Some(ConflictPlan::new(
        cell,
        outcome,
        &[],
        retained,
        CursorGate::NeedsReconcile,
    ))
}

fn classify_replay(replay: &ReplayAssessment) -> Option<ConflictPlan> {
    match replay {
        ReplayAssessment::Fresh => None,
        ReplayAssessment::VerifiedAppliedExactPostState(proof) => {
            if proof.operation_kind == MutationOperationKind::ConflictCopyPublish {
                Some(needs_reconcile(
                    ConflictCell::C29,
                    &[],
                    &[RetainedEvidence::PreserveBothEvidence],
                ))
            } else {
                Some(ConflictPlan::new(
                    ConflictCell::C29,
                    ConflictOutcome::NoOpVerified,
                    &[],
                    &[],
                    CursorGate::ExactVerification,
                ))
            }
        }
        ReplayAssessment::VerifiedNotApplied { retry, .. } => {
            let plan = match *retry {
                VerifiedNotAppliedRetry::GuardedLocalReplace => ConflictPlan::new(
                    ConflictCell::C30,
                    ConflictOutcome::GuardedLocalReplace,
                    &[ConflictDraft::LocalContentPublish],
                    &[],
                    CursorGate::AllLocalPublications,
                ),
                VerifiedNotAppliedRetry::GuardedConflictCopy => ConflictPlan::new(
                    ConflictCell::C30,
                    ConflictOutcome::PreserveBothLocal,
                    &[ConflictDraft::ConflictCopyPublish],
                    &[RetainedEvidence::PreserveBothEvidence],
                    CursorGate::AllLocalPublications,
                ),
                VerifiedNotAppliedRetry::RemoteExistingMutationBlocked => ConflictPlan::new(
                    ConflictCell::C30,
                    ConflictOutcome::RemoteMutationBlocked,
                    &[ConflictDraft::RemoteExistingBlocked],
                    &[],
                    CursorGate::NeedsReconcile,
                ),
                VerifiedNotAppliedRetry::PreconditionsChangedOrCapabilityUnavailable => {
                    needs_reconcile(ConflictCell::C30, &[], &[])
                }
            };
            Some(plan)
        }
        ReplayAssessment::SideEffectOutcomeUnknown(_) => {
            Some(needs_reconcile(ConflictCell::C31, &[], &[]))
        }
        ReplayAssessment::QueuedIntentCapturedBaseChanged(_) => Some(needs_reconcile(
            ConflictCell::C32,
            &[],
            &[RetainedEvidence::LocalBytes, RetainedEvidence::RemoteBytes],
        )),
    }
}

fn guarded_draft(guard: LocalRenameGuard, draft: ConflictDraft) -> &'static [ConflictDraft] {
    match (guard, draft) {
        (LocalRenameGuard::ExactIdentityLineageAndNoCollision, ConflictDraft::GuardedLocalMove) => {
            &[ConflictDraft::GuardedLocalMove]
        }
        (
            LocalRenameGuard::ExactIdentityLineageAndNoCollision,
            ConflictDraft::GuardedLocalRenameMove,
        ) => &[ConflictDraft::GuardedLocalRenameMove],
        _ => &[],
    }
}

fn preserve_if_available(
    available_cell: ConflictCell,
    unavailable_cell: ConflictCell,
    capability: GuardedLocalCapability,
) -> ConflictPlan {
    if capability.is_available() {
        preserve_both(available_cell)
    } else {
        needs_reconcile(
            unavailable_cell,
            &[],
            &[
                RetainedEvidence::LocalBytes,
                RetainedEvidence::RemoteBytes,
                RetainedEvidence::PreserveBothEvidence,
            ],
        )
    }
}

fn preserve_or_reconcile(cell: ConflictCell, capability: GuardedLocalCapability) -> ConflictPlan {
    if capability.is_available() {
        preserve_both(cell)
    } else {
        needs_reconcile(
            cell,
            &[],
            &[
                RetainedEvidence::LocalBytes,
                RetainedEvidence::RemoteBytes,
                RetainedEvidence::PreserveBothEvidence,
            ],
        )
    }
}

fn preserve_both(cell: ConflictCell) -> ConflictPlan {
    ConflictPlan::new(
        cell,
        ConflictOutcome::PreserveBothLocal,
        &[ConflictDraft::ConflictCopyPublish],
        &[
            RetainedEvidence::LocalBytes,
            RetainedEvidence::RemoteBytes,
            RetainedEvidence::PreserveBothEvidence,
        ],
        CursorGate::AllLocalPublications,
    )
}

fn needs_reconcile(
    cell: ConflictCell,
    drafts: &[ConflictDraft],
    retained: &[RetainedEvidence],
) -> ConflictPlan {
    ConflictPlan::new(
        cell,
        ConflictOutcome::NeedsReconcile,
        drafts,
        retained,
        CursorGate::NeedsReconcile,
    )
}
