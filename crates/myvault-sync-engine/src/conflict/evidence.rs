use uuid::Uuid;

use crate::ConflictEvidence;

use super::{
    normalized_collision_key, ConflictCell, ConflictOutcome, ContentFingerprint,
    MaterializationPlan, CONFLICT_NAMING_VERSION,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictEvidenceInput {
    pub operation_id: Uuid,
    pub explanation_code: Option<String>,
    pub device_alias: Option<String>,
    pub captured_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictEvidenceFailure {
    OperationNotInPlan,
    MissingDestinationEvidence,
    InvalidDestinationPath,
    InvalidEvidenceCode,
    InvalidEvidenceReference,
}

/// Builds the existing R3.1 immutable evidence envelope from a pure R3.2 plan.
///
/// Explanation/device/time remain caller-authored because they are explanatory only.
/// Every durable correctness code is derived from the sealed classification/materialization plan.
/// Device/time remain outside R3.1's canonical fingerprint.
/// Store validation and ownership checks remain the persistence boundary.
///
/// # Errors
/// Rejects an operation ID that is not one of the materialization plan's durable drafts.
pub fn conflict_evidence_from_plan(
    materialization: &MaterializationPlan,
    input: &ConflictEvidenceInput,
) -> Result<ConflictEvidence, ConflictEvidenceFailure> {
    validate_input(input)?;
    let operation_draft = materialization
        .drafts
        .iter()
        .find(|draft| {
            draft.operation_id == input.operation_id && draft.durable_operation_kind.is_some()
        })
        .ok_or(ConflictEvidenceFailure::OperationNotInPlan)?;
    let conflict_copy = materialization
        .drafts
        .iter()
        .find(|draft| draft.kind == super::LocalPlanKind::ConflictCopyPublish);
    let destination = conflict_copy
        .unwrap_or(operation_draft)
        .destination
        .as_ref()
        .ok_or(ConflictEvidenceFailure::MissingDestinationEvidence)?;
    let collision_key = normalized_collision_key(&destination.portable_path)
        .map_err(|_| ConflictEvidenceFailure::InvalidDestinationPath)?;
    let expected_copy = conflict_copy.and_then(|draft| draft.expected_content.as_ref());
    let derived = DerivedEvidenceCodes::from_plan(materialization);
    let mut evidence = ConflictEvidence {
        conflict_id: materialization.conflict_id.clone(),
        operation_id: input.operation_id,
        stable_cell_id: materialization.cell.as_str().to_owned(),
        local_state_code: derived.local_state.as_str().to_owned(),
        remote_state_code: derived.remote_state.as_str().to_owned(),
        content_class: derived.content_class.as_str().to_owned(),
        lineage_state: derived.lineage_state.as_str().to_owned(),
        classification_code: materialization.outcome.as_str().to_owned(),
        ambiguity_reason: derived.ambiguity_reason.as_str().to_owned(),
        evidence_sufficiency: derived.evidence_sufficiency.as_str().to_owned(),
        conflict_copy_operation_id: conflict_copy.map(|draft| draft.operation_id),
        base_evidence_id: None,
        local_evidence_id: None,
        remote_evidence_id: None,
        base_sha256: materialization
            .base
            .as_ref()
            .map(|source| source.content.sha256.clone()),
        base_byte_length: materialization
            .base
            .as_ref()
            .map(|source| source.content.byte_length),
        local_sha256: content_hash(materialization.local.as_ref().map(|source| &source.content)),
        local_byte_length: content_length(
            materialization.local.as_ref().map(|source| &source.content),
        ),
        remote_sha256: content_hash(
            materialization
                .remote
                .as_ref()
                .map(|source| &source.content),
        ),
        remote_byte_length: content_length(
            materialization
                .remote
                .as_ref()
                .map(|source| &source.content),
        ),
        naming_version: CONFLICT_NAMING_VERSION.to_owned(),
        normalized_collision_key: collision_key,
        target_parent_id: destination.parent_id.clone(),
        expected_conflict_copy_sha256: content_hash(expected_copy),
        expected_conflict_copy_byte_length: content_length(expected_copy),
        explanation_code: input.explanation_code.clone(),
        device_alias: input.device_alias.clone(),
        evidence_fingerprint: String::new(),
        captured_at_unix_ms: input.captured_at_unix_ms,
    };
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    Ok(evidence)
}

fn validate_input(input: &ConflictEvidenceInput) -> Result<(), ConflictEvidenceFailure> {
    if input
        .explanation_code
        .as_deref()
        .is_some_and(|value| !is_redacted_code(value))
        || input
            .device_alias
            .as_deref()
            .is_some_and(|value| !is_redacted_code(value))
    {
        return Err(ConflictEvidenceFailure::InvalidEvidenceCode);
    }
    if i64::try_from(input.captured_at_unix_ms).is_err() {
        return Err(ConflictEvidenceFailure::InvalidEvidenceReference);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct DerivedEvidenceCodes {
    local_state: SideState,
    remote_state: SideState,
    content_class: ContentClass,
    lineage_state: LineageState,
    ambiguity_reason: AmbiguityReason,
    evidence_sufficiency: EvidenceSufficiency,
}

impl DerivedEvidenceCodes {
    fn from_plan(plan: &MaterializationPlan) -> Self {
        let facts = plan.classification_evidence.facts();
        Self {
            local_state: SideState::derive(
                plan.cell,
                true,
                facts.base.as_ref(),
                facts.local.as_ref(),
            ),
            remote_state: SideState::derive(
                plan.cell,
                false,
                facts.base.as_ref(),
                facts.remote.as_ref(),
            ),
            content_class: ContentClass::derive(plan),
            lineage_state: LineageState::derive(plan),
            ambiguity_reason: AmbiguityReason::derive(plan),
            evidence_sufficiency: EvidenceSufficiency::derive(plan.outcome),
        }
    }
}

#[derive(Clone, Copy)]
enum SideState {
    Deleted,
    NotCaptured,
    PresentWithoutBase,
    UnchangedFromBase,
    ChangedFromBase,
}

impl SideState {
    fn derive(
        cell: ConflictCell,
        local: bool,
        base: Option<&ContentFingerprint>,
        side: Option<&ContentFingerprint>,
    ) -> Self {
        let deleted = if local {
            matches!(
                cell,
                ConflictCell::C09 | ConflictCell::C11 | ConflictCell::C21
            )
        } else {
            matches!(
                cell,
                ConflictCell::C10 | ConflictCell::C11 | ConflictCell::C20
            )
        };
        if deleted {
            return Self::Deleted;
        }
        match (base, side) {
            (_, None) => Self::NotCaptured,
            (None, Some(_)) => Self::PresentWithoutBase,
            (Some(base), Some(side)) if base == side => Self::UnchangedFromBase,
            (Some(_), Some(_)) => Self::ChangedFromBase,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::NotCaptured => "not_captured",
            Self::PresentWithoutBase => "present_without_base",
            Self::UnchangedFromBase => "unchanged_from_base",
            Self::ChangedFromBase => "changed_from_base",
        }
    }
}

#[derive(Clone, Copy)]
enum ContentClass {
    MarkdownVerified,
    Text,
    Binary,
    ContentFingerprintOnly,
    MetadataOnly,
}

impl ContentClass {
    fn derive(plan: &MaterializationPlan) -> Self {
        match plan.cell {
            ConflictCell::C03 => Self::MarkdownVerified,
            ConflictCell::C04 | ConflictCell::C07 | ConflictCell::C08 => Self::Text,
            ConflictCell::C05 => Self::Binary,
            _ if plan.classification_evidence.facts().base.is_some()
                || plan.classification_evidence.facts().local.is_some()
                || plan.classification_evidence.facts().remote.is_some() =>
            {
                Self::ContentFingerprintOnly
            }
            _ => Self::MetadataOnly,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::MarkdownVerified => "markdown_verified",
            Self::Text => "text",
            Self::Binary => "binary",
            Self::ContentFingerprintOnly => "content_fingerprint_only",
            Self::MetadataOnly => "metadata_only",
        }
    }
}

#[derive(Clone, Copy)]
enum LineageState {
    ExactIdentity,
    SameTarget,
    Divergent,
    CycleRejected,
    DestinationCollision,
    ParentChanged,
}

impl LineageState {
    fn derive(plan: &MaterializationPlan) -> Self {
        if plan.cell == ConflictCell::C18
            && plan
                .retained
                .contains(&super::RetainedEvidence::LocalRenameIntent)
        {
            return Self::CycleRejected;
        }
        match plan.cell {
            ConflictCell::C14a | ConflictCell::C17 => Self::SameTarget,
            ConflictCell::C14c | ConflictCell::C18 | ConflictCell::C19 => Self::Divergent,
            ConflictCell::C14b
            | ConflictCell::C22a
            | ConflictCell::C22b
            | ConflictCell::C23
            | ConflictCell::C24
            | ConflictCell::C25
            | ConflictCell::C34 => Self::DestinationCollision,
            ConflictCell::C26 => Self::ParentChanged,
            _ => Self::ExactIdentity,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::ExactIdentity => "exact_identity",
            Self::SameTarget => "same_target",
            Self::Divergent => "divergent",
            Self::CycleRejected => "cycle_rejected",
            Self::DestinationCollision => "destination_collision",
            Self::ParentChanged => "parent_changed",
        }
    }
}

#[derive(Clone, Copy)]
enum AmbiguityReason {
    None,
    TextOverlap,
    MissingExactBase,
    AmbiguousText,
    MergeBoundsExceeded,
    PortableNameCollision,
    DivergentRename,
    DivergentMove,
    RenameMoveConflict,
    RenameMoveCycle,
    DestinationCollision,
    DuplicateRemotePath,
    ParentLineageChanged,
}

impl AmbiguityReason {
    fn derive(plan: &MaterializationPlan) -> Self {
        if plan.cell == ConflictCell::C18
            && plan
                .retained
                .contains(&super::RetainedEvidence::LocalRenameIntent)
        {
            return Self::RenameMoveCycle;
        }
        match plan.cell {
            ConflictCell::C04 => Self::TextOverlap,
            ConflictCell::C06a | ConflictCell::C06b => Self::MissingExactBase,
            ConflictCell::C07 => Self::AmbiguousText,
            ConflictCell::C08 => Self::MergeBoundsExceeded,
            ConflictCell::C14b | ConflictCell::C34 => Self::PortableNameCollision,
            ConflictCell::C14c => Self::DivergentRename,
            ConflictCell::C18 => Self::DivergentMove,
            ConflictCell::C19 => Self::RenameMoveConflict,
            ConflictCell::C22a | ConflictCell::C22b | ConflictCell::C23 | ConflictCell::C24 => {
                Self::DestinationCollision
            }
            ConflictCell::C25 => Self::DuplicateRemotePath,
            ConflictCell::C26 => Self::ParentLineageChanged,
            _ => Self::None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::TextOverlap => "text_overlap",
            Self::MissingExactBase => "missing_exact_base",
            Self::AmbiguousText => "ambiguous_text",
            Self::MergeBoundsExceeded => "merge_bounds_exceeded",
            Self::PortableNameCollision => "portable_name_collision",
            Self::DivergentRename => "divergent_rename",
            Self::DivergentMove => "divergent_move",
            Self::RenameMoveConflict => "rename_move_conflict",
            Self::RenameMoveCycle => "rename_move_cycle",
            Self::DestinationCollision => "destination_collision",
            Self::DuplicateRemotePath => "duplicate_remote_path",
            Self::ParentLineageChanged => "parent_lineage_changed",
        }
    }
}

#[derive(Clone, Copy)]
enum EvidenceSufficiency {
    Exact,
    PreserveBothComplete,
    RemoteBlockedComplete,
    ReconcileComplete,
    BoundaryRejected,
}

impl EvidenceSufficiency {
    const fn derive(outcome: ConflictOutcome) -> Self {
        match outcome {
            ConflictOutcome::NoOpVerified
            | ConflictOutcome::GuardedLocalReplace
            | ConflictOutcome::SafeTextMergeLocal => Self::Exact,
            ConflictOutcome::PreserveBothLocal => Self::PreserveBothComplete,
            ConflictOutcome::RemoteMutationBlocked => Self::RemoteBlockedComplete,
            ConflictOutcome::NeedsReconcile => Self::ReconcileComplete,
            ConflictOutcome::UnsupportedProtected => Self::BoundaryRejected,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::PreserveBothComplete => "preserve_both_complete",
            Self::RemoteBlockedComplete => "remote_blocked_complete",
            Self::ReconcileComplete => "reconcile_complete",
            Self::BoundaryRejected => "boundary_rejected",
        }
    }
}

fn content_hash(value: Option<&ContentFingerprint>) -> Option<String> {
    value.map(|content| content.sha256.clone())
}

fn content_length(value: Option<&ContentFingerprint>) -> Option<u64> {
    value.map(|content| content.byte_length)
}

fn is_redacted_code(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conflict::{
        classify_conflict, materialize_conflict_plan, ClassificationEvidence,
        ClassificationEvidenceFacts, ConflictCase, ConflictInput, MaterializationContext,
        PublicationSource, PublicationTarget,
    };

    fn fingerprint(byte: u8) -> ContentFingerprint {
        ContentFingerprint {
            sha256: format!("{byte:02x}").repeat(32),
            byte_length: u64::from(byte),
        }
    }

    fn source(name: &str, byte: u8) -> PublicationSource {
        PublicationSource {
            portable_path: format!("notes/{name}.md"),
            parent_id: "parent-1".to_owned(),
            object_id: format!("object-{name}"),
            revision: Some(format!("{byte:02x}").repeat(32)),
            content: fingerprint(byte),
        }
    }

    #[test]
    fn r3_1_fingerprint_excludes_explanatory_device_and_time() {
        let classification = classify_conflict(
            ConflictInput::fresh(
                ClassificationEvidence::new(ClassificationEvidenceFacts {
                    account_id: Some("account-1".to_owned()),
                    remote_root_id: Some("root-1".to_owned()),
                    object_identity: Some("object-base".to_owned()),
                    canonical_identity_path: Some("notes/base.md".to_owned()),
                    target_parent_id: Some("parent-1".to_owned()),
                    base: Some(fingerprint(1)),
                    local: Some(fingerprint(2)),
                    remote: Some(fingerprint(1)),
                    local_revision: Some("02".repeat(32)),
                    remote_revision: Some("01".repeat(32)),
                    durable_state_version: 1,
                    prior_operation_id: None,
                    prior_outcome_fingerprint: None,
                    prior_intent_fingerprint: None,
                    prior_replay_disposition: None,
                    guarded_metadata_target: None,
                }),
                ConflictCase::LocalContentChanged,
            )
            .expect("classification input"),
        );
        let materialization = materialize_conflict_plan(
            &classification,
            &MaterializationContext {
                account_id: "account-1".to_owned(),
                remote_root_id: "root-1".to_owned(),
                base_reference: Some("base-1".to_owned()),
                durable_state_version: 1,
                base_local_revision: Some("11".repeat(32)),
                base_remote_revision: Some("remote-base-1".to_owned()),
                base: Some(source("base", 1)),
                local: Some(source("local", 2)),
                remote: Some(source("remote", 1)),
                local_target: PublicationTarget {
                    portable_path: "notes/local.md".to_owned(),
                    parent_id: "parent-1".to_owned(),
                    object_id: Some("object-local".to_owned()),
                },
                conflict_copy_destination_parent_path: None,
                occupied_conflict_copies: Vec::new(),
                rename_target: None,
                blocked_remote_source: Some(source("local", 2)),
                blocked_remote_destination: Some(PublicationTarget {
                    portable_path: "notes/local.md".to_owned(),
                    parent_id: "parent-1".to_owned(),
                    object_id: Some("object-local".to_owned()),
                }),
                merged_content: None,
            },
        )
        .expect("materialization");
        let operation_id = materialization.drafts[0].operation_id;
        let mut input = ConflictEvidenceInput {
            operation_id,
            explanation_code: Some("blocked_by_option_a".to_owned()),
            device_alias: Some("device-a".to_owned()),
            captured_at_unix_ms: 10,
        };
        let first = conflict_evidence_from_plan(&materialization, &input).expect("evidence");
        input.device_alias = Some("device-b".to_owned());
        input.captured_at_unix_ms = 20;
        let second = conflict_evidence_from_plan(&materialization, &input).expect("evidence rerun");
        assert_eq!(first.evidence_fingerprint, second.evidence_fingerprint);
        assert_eq!(first.classification_code, "remote_mutation_blocked");
        assert_eq!(first.local_state_code, "changed_from_base");
        assert_eq!(first.remote_state_code, "unchanged_from_base");
        assert_eq!(first.content_class, "content_fingerprint_only");
        assert_eq!(first.lineage_state, "exact_identity");
        assert_eq!(first.ambiguity_reason, "none");
        assert_eq!(first.evidence_sufficiency, "remote_blocked_complete");
        assert_eq!(first.evidence_fingerprint, first.canonical_fingerprint());
        assert_eq!(first.base_evidence_id, None);
        assert_eq!(first.local_evidence_id, None);
        assert_eq!(first.remote_evidence_id, None);

        input.explanation_code = Some("caller supplied prose".to_owned());
        assert_eq!(
            conflict_evidence_from_plan(&materialization, &input),
            Err(ConflictEvidenceFailure::InvalidEvidenceCode)
        );
        input.explanation_code = None;
        input.operation_id = Uuid::nil();
        assert_eq!(
            conflict_evidence_from_plan(&materialization, &input),
            Err(ConflictEvidenceFailure::OperationNotInPlan)
        );
    }
}
