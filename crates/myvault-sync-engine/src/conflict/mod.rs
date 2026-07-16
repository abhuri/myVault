mod classifier;
mod evidence;
mod identity;
mod markdown;
mod materialization;

pub use classifier::{
    classify_conflict, BoundaryAssessment, ClassificationEvidence, ClassificationEvidenceFacts,
    ClassificationProofFailure, ConflictCase, ConflictCell, ConflictDraft, ConflictInput,
    ConflictInputFailure, ConflictOutcome, ConflictPlan, CursorGate, DeletedFinalStateProof,
    GuardedLocalCapability, GuardedMetadataTargetEvidence, LocalRenameGuard, ReplayAssessment,
    ReplayDisposition, ReplayProof, ReplayProofFailure, RetainedEvidence, VerifiedNotAppliedRetry,
};
pub use evidence::{conflict_evidence_from_plan, ConflictEvidenceFailure, ConflictEvidenceInput};
pub use identity::{
    derive_conflict_id, derive_operation_id, normalized_collision_key, operation_marker,
    resolve_conflict_copy_name, ConflictCopyName, ConflictCopyNameOutcome, ConflictCopyNameRequest,
    ConflictIdentityInput, ConflictNameFailure, ConflictOperationDomain, ContentFingerprint,
    OccupiedConflictCopy, CONFLICT_ID_VERSION, CONFLICT_NAMING_VERSION,
};
pub use markdown::{
    merge_markdown_three_way, MarkdownMergeIssue, MarkdownMergeOutcome, MarkdownVersion,
    MergedMarkdown, NewlineStyle, MAX_MARKDOWN_COMBINED_BYTES, MAX_MARKDOWN_DIFF_WORK,
    MAX_MARKDOWN_LOGICAL_LINES, MAX_MARKDOWN_VERSION_BYTES,
};
pub use materialization::{
    materialize_conflict_plan, mutation_intent_from_draft, LocalPlanKind, MaterializationContext,
    MaterializationDraft, MaterializationFailure, MaterializationPlan, PublicationOrderDependency,
    PublicationSource, PublicationTarget,
};
