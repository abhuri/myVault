use uuid::Uuid;

use crate::{
    ChangeBatchDependency, ChangeBatchDependencyKind, MutationIntent, MutationOperationKind,
};

use super::identity::is_exact_content_path;
use super::{
    derive_conflict_id, derive_operation_id, operation_marker, resolve_conflict_copy_name,
    ClassificationEvidence, ConflictCell, ConflictCopyNameOutcome, ConflictCopyNameRequest,
    ConflictDraft, ConflictIdentityInput, ConflictOperationDomain, ConflictOutcome, ConflictPlan,
    ContentFingerprint, CursorGate, OccupiedConflictCopy, RetainedEvidence,
    CONFLICT_NAMING_VERSION,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationSource {
    pub portable_path: String,
    pub parent_id: String,
    pub object_id: String,
    pub revision: Option<String>,
    pub content: ContentFingerprint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationTarget {
    pub portable_path: String,
    pub parent_id: String,
    pub object_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationContext {
    pub account_id: String,
    pub remote_root_id: String,
    pub base_reference: Option<String>,
    pub durable_state_version: u64,
    pub base_local_revision: Option<String>,
    pub base_remote_revision: Option<String>,
    pub base: Option<PublicationSource>,
    pub local: Option<PublicationSource>,
    pub remote: Option<PublicationSource>,
    pub local_target: PublicationTarget,
    pub conflict_copy_destination_parent_path: Option<String>,
    pub occupied_conflict_copies: Vec<OccupiedConflictCopy>,
    pub rename_target: Option<PublicationTarget>,
    pub blocked_remote_source: Option<PublicationSource>,
    pub blocked_remote_destination: Option<PublicationTarget>,
    pub merged_content: Option<ContentFingerprint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalPlanKind {
    LocalPublish,
    MergePublish,
    ConflictCopyPublish,
    ConflictCopyReuseVerified,
    BasePublish,
    RemoteExistingBlocked,
    GuardedLocalRename,
    GuardedLocalMove,
    GuardedLocalRenameMove,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationDraft {
    pub(crate) operation_id: Uuid,
    pub(crate) kind: LocalPlanKind,
    pub(crate) durable_operation_kind: Option<MutationOperationKind>,
    pub(crate) source: Option<PublicationSource>,
    pub(crate) destination: Option<PublicationTarget>,
    pub(crate) expected_content: Option<ContentFingerprint>,
    pub(crate) operation_marker: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationPlan {
    pub(crate) conflict_id: String,
    pub(crate) cell: ConflictCell,
    pub(crate) outcome: ConflictOutcome,
    pub(crate) cursor_gate: CursorGate,
    pub(crate) retained: Vec<RetainedEvidence>,
    pub(crate) classification_evidence: ClassificationEvidence,
    pub(crate) account_id: String,
    pub(crate) remote_root_id: String,
    pub(crate) base_reference: Option<String>,
    pub(crate) durable_state_version: u64,
    pub(crate) base_local_revision: Option<String>,
    pub(crate) base_remote_revision: Option<String>,
    pub(crate) base: Option<PublicationSource>,
    pub(crate) local: Option<PublicationSource>,
    pub(crate) remote: Option<PublicationSource>,
    pub(crate) drafts: Vec<MaterializationDraft>,
    pub(crate) cursor_dependencies: Vec<ChangeBatchDependency>,
    pub(crate) execution_dependencies: Vec<PublicationOrderDependency>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationOrderDependency {
    pub(crate) operation_id: Uuid,
    pub(crate) prerequisites: Vec<Uuid>,
}

impl MaterializationDraft {
    #[must_use]
    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    #[must_use]
    pub const fn kind(&self) -> LocalPlanKind {
        self.kind
    }
    #[must_use]
    pub const fn durable_operation_kind(&self) -> Option<MutationOperationKind> {
        self.durable_operation_kind
    }
    #[must_use]
    pub const fn source(&self) -> Option<&PublicationSource> {
        self.source.as_ref()
    }
    #[must_use]
    pub const fn destination(&self) -> Option<&PublicationTarget> {
        self.destination.as_ref()
    }
    #[must_use]
    pub const fn expected_content(&self) -> Option<&ContentFingerprint> {
        self.expected_content.as_ref()
    }
    #[must_use]
    pub fn operation_marker(&self) -> &str {
        &self.operation_marker
    }
}

impl MaterializationPlan {
    #[must_use]
    pub fn conflict_id(&self) -> &str {
        &self.conflict_id
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
    pub const fn cursor_gate(&self) -> CursorGate {
        self.cursor_gate
    }
    #[must_use]
    pub fn retained(&self) -> &[RetainedEvidence] {
        &self.retained
    }
    #[must_use]
    pub const fn classification_evidence(&self) -> &ClassificationEvidence {
        &self.classification_evidence
    }
    #[must_use]
    pub fn base(&self) -> Option<&PublicationSource> {
        self.base.as_ref()
    }
    #[must_use]
    pub fn local(&self) -> Option<&PublicationSource> {
        self.local.as_ref()
    }
    #[must_use]
    pub fn remote(&self) -> Option<&PublicationSource> {
        self.remote.as_ref()
    }
    #[must_use]
    pub fn drafts(&self) -> &[MaterializationDraft] {
        &self.drafts
    }
    #[must_use]
    pub fn cursor_dependencies(&self) -> &[ChangeBatchDependency] {
        &self.cursor_dependencies
    }
    #[must_use]
    pub fn execution_dependencies(&self) -> &[PublicationOrderDependency] {
        &self.execution_dependencies
    }
}

impl PublicationOrderDependency {
    #[must_use]
    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    #[must_use]
    pub fn prerequisites(&self) -> &[Uuid] {
        &self.prerequisites
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaterializationFailure {
    InvalidConflictId,
    MissingLocalEvidence,
    MissingRemoteEvidence,
    MissingMergedContent,
    MissingConflictCopyTarget,
    MissingResolvedBase,
    MissingRenameTarget,
    MissingBlockedIntentEvidence,
    InvalidEvidence,
    InvalidRegisteredTimestamp,
    DraftNotInPlan,
    ClassificationEvidenceMismatch,
}

/// Converts a pure classification into immutable local/durable operation drafts.
///
/// `RemoteExistingBlocked` deliberately has no cursor dependency and no executable
/// provider capability. A consumer must register it directly into `NeedsReconcile`.
///
/// # Errors
/// Returns a typed failure when exact evidence required by an emitted draft is absent.
pub fn materialize_conflict_plan(
    classification: &ConflictPlan,
    context: &MaterializationContext,
) -> Result<MaterializationPlan, MaterializationFailure> {
    validate_context(context)?;
    validate_classification_binding(classification, context)?;
    let conflict_id = derive_bound_conflict_id(classification)?;
    let mut drafts = Vec::with_capacity(classification.drafts.len());
    let mut cursor_dependencies = Vec::new();
    for requirement in &classification.drafts {
        let (draft, dependency) = draft_for(*requirement, classification, context, &conflict_id)?;
        if let Some(dependency) = dependency {
            cursor_dependencies.push(dependency);
        }
        drafts.push(draft);
    }
    let publication_prerequisites = drafts
        .iter()
        .filter(|draft| {
            matches!(
                draft.durable_operation_kind,
                Some(
                    MutationOperationKind::LocalPublish
                        | MutationOperationKind::MergePublish
                        | MutationOperationKind::ConflictCopyPublish
                )
            )
        })
        .map(|draft| draft.operation_id)
        .collect::<Vec<_>>();
    let execution_dependencies = drafts
        .iter()
        .map(|draft| PublicationOrderDependency {
            operation_id: draft.operation_id,
            prerequisites: if draft.durable_operation_kind
                == Some(MutationOperationKind::BasePublish)
            {
                publication_prerequisites.clone()
            } else {
                Vec::new()
            },
        })
        .collect();
    let cursor_gate = if drafts
        .iter()
        .any(|draft| draft.kind == LocalPlanKind::ConflictCopyReuseVerified)
    {
        CursorGate::NeedsReconcile
    } else {
        classification.cursor_gate
    };
    Ok(MaterializationPlan {
        conflict_id,
        cell: classification.cell,
        outcome: classification.outcome,
        cursor_gate,
        retained: classification.retained.clone(),
        classification_evidence: classification.evidence().clone(),
        account_id: context.account_id.clone(),
        remote_root_id: context.remote_root_id.clone(),
        base_reference: context.base_reference.clone(),
        durable_state_version: context.durable_state_version,
        base_local_revision: context.base_local_revision.clone(),
        base_remote_revision: context.base_remote_revision.clone(),
        base: context.base.clone(),
        local: context.local.clone(),
        remote: context.remote.clone(),
        drafts,
        cursor_dependencies,
        execution_dependencies,
    })
}

/// Finalizes one pure draft into the existing R3.1 immutable intent envelope.
/// Guarded metadata-only drafts return `Ok(None)` because R3.1 intentionally has
/// no rename/move execution capability.
///
/// # Errors
/// Returns a typed failure if a durable draft is missing its declared operation kind.
pub fn mutation_intent_from_draft(
    plan: &MaterializationPlan,
    draft: &MaterializationDraft,
    registered_at_unix_ms: u64,
) -> Result<Option<MutationIntent>, MaterializationFailure> {
    if i64::try_from(registered_at_unix_ms).is_err() {
        return Err(MaterializationFailure::InvalidRegisteredTimestamp);
    }
    if !plan.drafts.iter().any(|planned| planned == draft) {
        return Err(MaterializationFailure::DraftNotInPlan);
    }
    let Some(operation_kind) = draft.durable_operation_kind else {
        return Ok(None);
    };
    let mut intent = MutationIntent {
        operation_id: draft.operation_id,
        operation_kind,
        account_id: Some(plan.account_id.clone()),
        remote_root_id: Some(plan.remote_root_id.clone()),
        remote_file_id: plan.remote.as_ref().map(|remote| remote.object_id.clone()),
        source_parent_id: draft.source.as_ref().map(|source| source.parent_id.clone()),
        destination_parent_id: draft
            .destination
            .as_ref()
            .map(|target| target.parent_id.clone()),
        local_object_id: plan.local.as_ref().map(|local| local.object_id.clone()),
        source_path: draft
            .source
            .as_ref()
            .map(|source| source.portable_path.clone()),
        destination_path: draft
            .destination
            .as_ref()
            .map(|target| target.portable_path.clone()),
        expected_local_revision: plan.local.as_ref().and_then(|local| local.revision.clone()),
        expected_remote_revision: plan
            .remote
            .as_ref()
            .and_then(|remote| remote.revision.clone()),
        base_reference: plan.base_reference.clone(),
        base_local_revision: plan.base_local_revision.clone(),
        base_remote_revision: plan.base_remote_revision.clone(),
        base_sha256: plan.base.as_ref().map(|base| base.content.sha256.clone()),
        base_byte_length: plan.base.as_ref().map(|base| base.content.byte_length),
        expected_local_sha256: draft
            .expected_content
            .as_ref()
            .map(|content| content.sha256.clone()),
        expected_local_byte_length: draft
            .expected_content
            .as_ref()
            .map(|content| content.byte_length),
        expected_remote_sha256: plan
            .remote
            .as_ref()
            .map(|remote| remote.content.sha256.clone()),
        expected_remote_byte_length: plan
            .remote
            .as_ref()
            .map(|remote| remote.content.byte_length),
        operation_marker: draft.operation_marker.clone(),
        intent_fingerprint: String::new(),
        registered_at_unix_ms,
    };
    intent.intent_fingerprint = intent.canonical_fingerprint();
    Ok(Some(intent))
}

fn validate_context(context: &MaterializationContext) -> Result<(), MaterializationFailure> {
    if context.durable_state_version == 0
        || !is_remote_id(&context.account_id)
        || !is_remote_id(&context.remote_root_id)
        || context
            .base_reference
            .as_deref()
            .is_some_and(|value| !is_private_reference(value))
        || context
            .base_local_revision
            .as_deref()
            .is_some_and(|value| !is_lower_hex_64(value))
        || context
            .base_remote_revision
            .as_deref()
            .is_some_and(|value| !is_remote_id(value))
        || context
            .local
            .as_ref()
            .and_then(|source| source.revision.as_deref())
            .is_some_and(|value| !is_lower_hex_64(value))
        || context
            .local
            .as_ref()
            .is_some_and(|source| !is_private_reference(&source.object_id))
        || context
            .remote
            .as_ref()
            .and_then(|source| source.revision.as_deref())
            .is_some_and(|value| !is_remote_id(value))
        || [
            context.base.as_ref(),
            context.local.as_ref(),
            context.remote.as_ref(),
            context.blocked_remote_source.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|source| {
            !valid_path(&source.portable_path)
                || !is_remote_id(&source.parent_id)
                || !is_remote_id(&source.object_id)
                || !valid_content(&source.content)
        })
        || [
            Some(&context.local_target),
            context.rename_target.as_ref(),
            context.blocked_remote_destination.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|target| {
            !valid_path(&target.portable_path)
                || !is_remote_id(&target.parent_id)
                || target
                    .object_id
                    .as_deref()
                    .is_some_and(|value| !is_remote_id(value))
        })
        || context
            .conflict_copy_destination_parent_path
            .as_deref()
            .is_some_and(|path| !valid_path(path))
        || [context.merged_content.as_ref()]
            .into_iter()
            .flatten()
            .any(|content| !valid_content(content))
    {
        return Err(MaterializationFailure::InvalidEvidence);
    }
    Ok(())
}

fn derive_bound_conflict_id(
    classification: &ConflictPlan,
) -> Result<String, MaterializationFailure> {
    let facts = classification.evidence().facts();
    derive_conflict_id(&ConflictIdentityInput {
        account_id: facts
            .account_id
            .clone()
            .ok_or(MaterializationFailure::ClassificationEvidenceMismatch)?,
        remote_root_id: facts
            .remote_root_id
            .clone()
            .ok_or(MaterializationFailure::ClassificationEvidenceMismatch)?,
        object_identity: facts
            .object_identity
            .clone()
            .ok_or(MaterializationFailure::ClassificationEvidenceMismatch)?,
        cell: classification.cell,
        outcome: classification.outcome,
        canonical_identity_path: facts
            .canonical_identity_path
            .clone()
            .ok_or(MaterializationFailure::ClassificationEvidenceMismatch)?,
        target_parent_id: facts
            .target_parent_id
            .clone()
            .ok_or(MaterializationFailure::ClassificationEvidenceMismatch)?,
        base: facts.base.clone(),
        local: facts.local.clone(),
        remote: facts.remote.clone(),
        naming_version: CONFLICT_NAMING_VERSION.to_owned(),
    })
    .map_err(|_| MaterializationFailure::InvalidConflictId)
}

fn validate_classification_binding(
    classification: &ConflictPlan,
    context: &MaterializationContext,
) -> Result<(), MaterializationFailure> {
    let evidence = classification.evidence();
    let facts = evidence.facts();
    let identity_source = context
        .base
        .as_ref()
        .or(context.local.as_ref())
        .or(context.remote.as_ref());
    let guarded_target_matches = if classification.drafts.iter().any(|draft| {
        matches!(
            draft,
            ConflictDraft::GuardedLocalRename
                | ConflictDraft::GuardedLocalMove
                | ConflictDraft::GuardedLocalRenameMove
        )
    }) {
        match (
            facts.guarded_metadata_target.as_ref(),
            context.rename_target.as_ref(),
        ) {
            (Some(evidence), Some(target)) => {
                evidence.portable_path == target.portable_path
                    && evidence.parent_id == target.parent_id
                    && evidence.object_id == target.object_id
            }
            _ => false,
        }
    } else {
        true
    };
    let bound = facts.account_id.as_deref() == Some(context.account_id.as_str())
        && facts.remote_root_id.as_deref() == Some(context.remote_root_id.as_str())
        && facts.object_identity.as_deref()
            == identity_source.map(|source| source.object_id.as_str())
        && facts.canonical_identity_path.as_deref()
            == identity_source.map(|source| source.portable_path.as_str())
        && facts.target_parent_id.as_deref() == Some(context.local_target.parent_id.as_str())
        && facts.base.as_ref() == context.base.as_ref().map(|source| &source.content)
        && facts.local.as_ref() == context.local.as_ref().map(|source| &source.content)
        && facts.remote.as_ref() == context.remote.as_ref().map(|source| &source.content)
        && facts.local_revision.as_ref()
            == context
                .local
                .as_ref()
                .and_then(|source| source.revision.as_ref())
        && facts.remote_revision.as_ref()
            == context
                .remote
                .as_ref()
                .and_then(|source| source.revision.as_ref())
        && facts.durable_state_version == context.durable_state_version
        && guarded_target_matches
        && (classification.outcome != ConflictOutcome::SafeTextMergeLocal
            || evidence.verified_markdown_merge() == context.merged_content.as_ref());
    if bound {
        Ok(())
    } else {
        Err(MaterializationFailure::ClassificationEvidenceMismatch)
    }
}

fn valid_path(value: &str) -> bool {
    is_exact_content_path(value)
}

fn is_remote_id(value: &str) -> bool {
    (1..=512).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn is_private_reference(value: &str) -> bool {
    (1..=256).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
        && !matches!(value, "." | "..")
}

fn valid_content(value: &ContentFingerprint) -> bool {
    is_lower_hex_64(&value.sha256) && i64::try_from(value.byte_length).is_ok()
}

#[allow(clippy::too_many_lines)]
fn draft_for(
    requirement: ConflictDraft,
    classification: &ConflictPlan,
    context: &MaterializationContext,
    conflict_id: &str,
) -> Result<(MaterializationDraft, Option<ChangeBatchDependency>), MaterializationFailure> {
    match requirement {
        ConflictDraft::LocalContentPublish => {
            let remote = context
                .remote
                .clone()
                .ok_or(MaterializationFailure::MissingRemoteEvidence)?;
            Ok(durable_draft(
                LocalPlanKind::LocalPublish,
                MutationOperationKind::LocalPublish,
                ConflictOperationDomain::LocalPublish,
                ChangeBatchDependencyKind::Mutation,
                Some(remote.clone()),
                Some(context.local_target.clone()),
                Some(remote.content),
                conflict_id,
            ))
        }
        ConflictDraft::MergePublish => {
            let merged_content = context
                .merged_content
                .clone()
                .ok_or(MaterializationFailure::MissingMergedContent)?;
            Ok(durable_draft(
                LocalPlanKind::MergePublish,
                MutationOperationKind::MergePublish,
                ConflictOperationDomain::MergePublish,
                ChangeBatchDependencyKind::MergePublication,
                context.local.clone(),
                Some(context.local_target.clone()),
                Some(merged_content),
                conflict_id,
            ))
        }
        ConflictDraft::ConflictCopyPublish => {
            let remote = context
                .remote
                .clone()
                .ok_or(MaterializationFailure::MissingRemoteEvidence)?;
            let outcome = resolve_conflict_copy_name(&ConflictCopyNameRequest {
                conflict_id: conflict_id.to_owned(),
                source_path: remote.portable_path.clone(),
                destination_parent_path: context.conflict_copy_destination_parent_path.clone(),
                expected_content: remote.content.clone(),
                naming_version: CONFLICT_NAMING_VERSION.to_owned(),
                occupied: context.occupied_conflict_copies.clone(),
            });
            let (name, reuse_verified) = match outcome {
                ConflictCopyNameOutcome::Create(name) => (name, false),
                ConflictCopyNameOutcome::Reuse(name) => (name, true),
                ConflictCopyNameOutcome::NeedsReconcile(_) => {
                    return Err(MaterializationFailure::MissingConflictCopyTarget);
                }
            };
            let target = PublicationTarget {
                portable_path: name.destination_path,
                parent_id: context.local_target.parent_id.clone(),
                object_id: name.existing_object_id,
            };
            if reuse_verified {
                let operation_id =
                    derive_operation_id(ConflictOperationDomain::ConflictCopy, conflict_id);
                return Ok((
                    MaterializationDraft {
                        operation_id,
                        kind: LocalPlanKind::ConflictCopyReuseVerified,
                        durable_operation_kind: None,
                        source: Some(remote.clone()),
                        destination: Some(target),
                        expected_content: Some(remote.content),
                        operation_marker: operation_marker(
                            ConflictOperationDomain::ConflictCopy,
                            conflict_id,
                        ),
                    },
                    None,
                ));
            }
            Ok(durable_draft(
                LocalPlanKind::ConflictCopyPublish,
                MutationOperationKind::ConflictCopyPublish,
                ConflictOperationDomain::ConflictCopy,
                ChangeBatchDependencyKind::ConflictCopyPublication,
                Some(remote.clone()),
                Some(target),
                Some(remote.content),
                conflict_id,
            ))
        }
        ConflictDraft::BasePublish => {
            let base_content = match classification.outcome {
                ConflictOutcome::SafeTextMergeLocal => context.merged_content.clone(),
                ConflictOutcome::GuardedLocalReplace | ConflictOutcome::NeedsReconcile => {
                    context.remote.as_ref().map(|source| source.content.clone())
                }
                _ => None,
            }
            .ok_or(MaterializationFailure::MissingResolvedBase)?;
            let base_source = context.local.as_ref().map(|local| PublicationSource {
                portable_path: context.local_target.portable_path.clone(),
                parent_id: context.local_target.parent_id.clone(),
                object_id: context
                    .local_target
                    .object_id
                    .clone()
                    .unwrap_or_else(|| local.object_id.clone()),
                revision: local.revision.clone(),
                content: base_content.clone(),
            });
            Ok(durable_draft(
                LocalPlanKind::BasePublish,
                MutationOperationKind::BasePublish,
                ConflictOperationDomain::BasePublish,
                ChangeBatchDependencyKind::BasePublication,
                base_source,
                Some(context.local_target.clone()),
                Some(base_content),
                conflict_id,
            ))
        }
        ConflictDraft::RemoteExistingBlocked => {
            let source = context
                .blocked_remote_source
                .clone()
                .ok_or(MaterializationFailure::MissingBlockedIntentEvidence)?;
            let destination = context
                .blocked_remote_destination
                .clone()
                .ok_or(MaterializationFailure::MissingBlockedIntentEvidence)?;
            let operation_id =
                derive_operation_id(ConflictOperationDomain::RemoteExistingBlocked, conflict_id);
            Ok((
                MaterializationDraft {
                    operation_id,
                    kind: LocalPlanKind::RemoteExistingBlocked,
                    durable_operation_kind: Some(MutationOperationKind::RemoteExistingBlocked),
                    source: Some(source.clone()),
                    destination: Some(destination),
                    expected_content: Some(source.content),
                    operation_marker: operation_marker(
                        ConflictOperationDomain::RemoteExistingBlocked,
                        conflict_id,
                    ),
                },
                None,
            ))
        }
        ConflictDraft::GuardedLocalRename => guarded_metadata_draft(
            LocalPlanKind::GuardedLocalRename,
            ConflictOperationDomain::GuardedLocalRename,
            context,
            conflict_id,
        ),
        ConflictDraft::GuardedLocalMove => guarded_metadata_draft(
            LocalPlanKind::GuardedLocalMove,
            ConflictOperationDomain::GuardedLocalMove,
            context,
            conflict_id,
        ),
        ConflictDraft::GuardedLocalRenameMove => guarded_metadata_draft(
            LocalPlanKind::GuardedLocalRenameMove,
            ConflictOperationDomain::GuardedLocalRenameMove,
            context,
            conflict_id,
        ),
    }
}

fn guarded_metadata_draft(
    kind: LocalPlanKind,
    domain: ConflictOperationDomain,
    context: &MaterializationContext,
    conflict_id: &str,
) -> Result<(MaterializationDraft, Option<ChangeBatchDependency>), MaterializationFailure> {
    let target = context
        .rename_target
        .clone()
        .ok_or(MaterializationFailure::MissingRenameTarget)?;
    let operation_id = derive_operation_id(domain, conflict_id);
    Ok((
        MaterializationDraft {
            operation_id,
            kind,
            durable_operation_kind: None,
            source: context.local.clone(),
            destination: Some(target),
            expected_content: context.local.as_ref().map(|local| local.content.clone()),
            operation_marker: operation_marker(domain, conflict_id),
        },
        None,
    ))
}

#[allow(clippy::too_many_arguments)]
fn durable_draft(
    kind: LocalPlanKind,
    durable_operation_kind: MutationOperationKind,
    domain: ConflictOperationDomain,
    dependency_kind: ChangeBatchDependencyKind,
    source: Option<PublicationSource>,
    destination: Option<PublicationTarget>,
    expected_content: Option<ContentFingerprint>,
    conflict_id: &str,
) -> (MaterializationDraft, Option<ChangeBatchDependency>) {
    let operation_id = derive_operation_id(domain, conflict_id);
    (
        MaterializationDraft {
            operation_id,
            kind,
            durable_operation_kind: Some(durable_operation_kind),
            source,
            destination,
            expected_content,
            operation_marker: operation_marker(domain, conflict_id),
        },
        Some(ChangeBatchDependency {
            operation_id,
            kind: dependency_kind,
        }),
    )
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::conflict::{
        classify_conflict, ClassificationEvidence, ClassificationEvidenceFacts, ConflictCase,
        ConflictInput, ReplayAssessment, ReplayProof,
    };
    use crate::{
        MutationDisposition, MutationEvidenceCapturePhase, MutationPhase, MutationState,
        MutationVerificationEvidence,
    };

    fn fingerprint(byte: u8) -> ContentFingerprint {
        ContentFingerprint {
            sha256: format!("{byte:02x}").repeat(32),
            byte_length: u64::from(byte),
        }
    }

    fn content_fingerprint(content: &str) -> ContentFingerprint {
        use sha2::{Digest, Sha256};
        ContentFingerprint {
            sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
            byte_length: u64::try_from(content.len()).expect("fixture length fits u64"),
        }
    }

    fn classification_evidence() -> ClassificationEvidence {
        ClassificationEvidence::new(ClassificationEvidenceFacts {
            account_id: Some("account-1".to_owned()),
            remote_root_id: Some("root-1".to_owned()),
            object_identity: Some("object-1".to_owned()),
            canonical_identity_path: Some("notes/a.md".to_owned()),
            target_parent_id: Some("parent-1".to_owned()),
            base: Some(content_fingerprint("a\nb\n")),
            local: Some(content_fingerprint("A\nb\n")),
            remote: Some(content_fingerprint("a\nB\n")),
            local_revision: Some("02".repeat(32)),
            remote_revision: Some("03".repeat(32)),
            durable_state_version: 1,
            prior_operation_id: None,
            prior_outcome_fingerprint: None,
            prior_intent_fingerprint: None,
            prior_replay_disposition: None,
            guarded_metadata_target: Some(crate::conflict::GuardedMetadataTargetEvidence {
                portable_path: "notes/guarded.md".to_owned(),
                parent_id: "parent-1".to_owned(),
                object_id: Some("object-1".to_owned()),
                normalized_collision_key: crate::conflict::normalized_collision_key(
                    "notes/guarded.md",
                )
                .expect("guarded collision key"),
                occupied_collision_keys: Vec::new(),
            }),
        })
        .verify_markdown_merge("a\nb\n", "A\nb\n", "a\nB\n")
        .expect("verified merge")
    }

    fn merged_fingerprint() -> ContentFingerprint {
        let crate::conflict::MarkdownMergeOutcome::Merged(merged) =
            crate::conflict::merge_markdown_three_way("a\nb\n", "A\nb\n", "a\nB\n")
        else {
            panic!("verified merge fixture");
        };
        ContentFingerprint {
            sha256: merged.sha256,
            byte_length: merged.byte_length,
        }
    }

    fn classify(case: ConflictCase) -> ConflictPlan {
        let mut evidence = classification_evidence();
        if matches!(case, ConflictCase::RemoteContentChanged { .. }) {
            let mut facts = evidence.facts().clone();
            facts.local.clone_from(&facts.base);
            evidence = ClassificationEvidence::new(facts);
        } else if case == ConflictCase::LocalContentChanged {
            let mut facts = evidence.facts().clone();
            facts.remote.clone_from(&facts.base);
            evidence = ClassificationEvidence::new(facts);
        } else if matches!(case, ConflictCase::BothChangedWithoutExactBase { .. }) {
            let mut facts = evidence.facts().clone();
            facts.base = None;
            evidence = ClassificationEvidence::new(facts);
        }
        classify_conflict(ConflictInput::fresh(evidence, case).expect("classification input"))
    }

    fn source(name: &str, byte: u8) -> PublicationSource {
        let content = match name {
            "base" => content_fingerprint("a\nb\n"),
            "local" => content_fingerprint("A\nb\n"),
            "remote" => content_fingerprint("a\nB\n"),
            _ => fingerprint(byte),
        };
        PublicationSource {
            portable_path: "notes/a.md".to_owned(),
            parent_id: "parent-1".to_owned(),
            object_id: "object-1".to_owned(),
            revision: Some(format!("{byte:02x}").repeat(32)),
            content,
        }
    }

    fn context() -> MaterializationContext {
        MaterializationContext {
            account_id: "account-1".to_owned(),
            remote_root_id: "root-1".to_owned(),
            base_reference: Some("base-1".to_owned()),
            durable_state_version: 1,
            base_local_revision: Some("11".repeat(32)),
            base_remote_revision: Some("base-remote-revision".to_owned()),
            base: Some(source("base", 1)),
            local: Some(source("local", 2)),
            remote: Some(source("remote", 3)),
            local_target: PublicationTarget {
                portable_path: "notes/a.md".to_owned(),
                parent_id: "parent-1".to_owned(),
                object_id: Some("object-1".to_owned()),
            },
            conflict_copy_destination_parent_path: Some("notes".to_owned()),
            occupied_conflict_copies: Vec::new(),
            rename_target: None,
            blocked_remote_source: Some(source("local", 2)),
            blocked_remote_destination: Some(PublicationTarget {
                portable_path: "notes/a.md".to_owned(),
                parent_id: "parent-1".to_owned(),
                object_id: Some("object-1".to_owned()),
            }),
            merged_content: Some(merged_fingerprint()),
        }
    }

    fn context_for(classification: &ConflictPlan) -> MaterializationContext {
        let mut context = context();
        let facts = classification.evidence().facts();
        if let (Some(source), Some(content)) = (context.base.as_mut(), facts.base.as_ref()) {
            source.content = content.clone();
        }
        if let (Some(source), Some(content)) = (context.local.as_mut(), facts.local.as_ref()) {
            source.content = content.clone();
        }
        if let (Some(source), Some(content)) = (context.remote.as_mut(), facts.remote.as_ref()) {
            source.content = content.clone();
        }
        context
    }

    #[test]
    fn merge_plan_maps_only_local_publications_into_cursor_dependencies() {
        let classification = classify(ConflictCase::NonOverlappingMarkdownChanges);
        let plan = materialize_conflict_plan(&classification, &context()).expect("materialize");
        assert_eq!(plan.drafts.len(), 3);
        assert_eq!(plan.cursor_dependencies.len(), 2);
        assert_eq!(plan.cursor_gate, CursorGate::NeedsReconcile);
        assert!(plan.drafts.iter().any(|draft| {
            draft.kind == LocalPlanKind::RemoteExistingBlocked
                && draft.durable_operation_kind
                    == Some(MutationOperationKind::RemoteExistingBlocked)
        }));
        assert!(plan.cursor_dependencies.iter().all(|dependency| {
            matches!(
                dependency.kind,
                ChangeBatchDependencyKind::MergePublication
                    | ChangeBatchDependencyKind::BasePublication
            )
        }));
        let base = plan
            .execution_dependencies
            .iter()
            .find(|dependency| {
                plan.drafts.iter().any(|draft| {
                    draft.operation_id == dependency.operation_id
                        && draft.kind == LocalPlanKind::BasePublish
                })
            })
            .expect("base dependency");
        assert_eq!(base.prerequisites.len(), 1);
        assert_eq!(
            base.prerequisites[0],
            plan.drafts
                .iter()
                .find(|draft| draft.kind == LocalPlanKind::MergePublish)
                .expect("merge draft")
                .operation_id
        );
    }

    #[test]
    fn preserve_both_is_deterministic_and_requires_exact_copy_target() {
        let classification = classify(ConflictCase::BothBinaryChanged);
        let first = materialize_conflict_plan(&classification, &context()).expect("first");
        let second = materialize_conflict_plan(&classification, &context()).expect("second");
        assert_eq!(first, second);
        assert_eq!(first.cursor_gate, CursorGate::AllLocalPublications);
        assert_eq!(
            first.drafts[0].expected_content,
            context().remote.map(|source| source.content)
        );
        let mut incomplete = context();
        incomplete.conflict_copy_destination_parent_path = Some(".trash".to_owned());
        assert_eq!(
            materialize_conflict_plan(&classification, &incomplete),
            Err(MaterializationFailure::InvalidEvidence)
        );

        assert_eq!(first.drafts.len(), 1);
        assert_eq!(first.drafts[0].kind, LocalPlanKind::ConflictCopyPublish);
    }

    #[test]
    fn exact_conflict_copy_rerun_is_verification_only_without_create_intent() {
        let classification = classify(ConflictCase::BothBinaryChanged);
        let initial_context = context();
        let initial = materialize_conflict_plan(&classification, &initial_context).expect("create");
        let created = &initial.drafts[0];
        let destination = created.destination.as_ref().expect("destination");
        let mut rerun_context = initial_context;
        rerun_context
            .occupied_conflict_copies
            .push(OccupiedConflictCopy {
                normalized_collision_key: crate::conflict::normalized_collision_key(
                    &destination.portable_path,
                )
                .expect("collision key"),
                conflict_id: initial.conflict_id.clone(),
                expected_content: created.expected_content.clone().expect("content"),
                destination_path: destination.portable_path.clone(),
                object_id: "existing-conflict-copy".to_owned(),
            });
        let rerun = materialize_conflict_plan(&classification, &rerun_context).expect("reuse");
        assert_eq!(rerun.drafts.len(), 1);
        assert_eq!(
            rerun.drafts[0].kind,
            LocalPlanKind::ConflictCopyReuseVerified
        );
        assert_eq!(rerun.cursor_gate, CursorGate::NeedsReconcile);
        assert!(rerun.cursor_dependencies.is_empty());
        assert_eq!(
            rerun.drafts[0]
                .destination
                .as_ref()
                .and_then(|target| target.object_id.as_deref()),
            Some("existing-conflict-copy")
        );
        assert_eq!(
            mutation_intent_from_draft(&rerun, &rerun.drafts[0], 42),
            Ok(None)
        );
    }

    #[test]
    fn materialization_rejects_context_that_does_not_match_classification_proof() {
        let merge = classify(ConflictCase::NonOverlappingMarkdownChanges);

        let mut wrong_account = context();
        wrong_account.account_id = "account-2".to_owned();
        assert_eq!(
            materialize_conflict_plan(&merge, &wrong_account),
            Err(MaterializationFailure::ClassificationEvidenceMismatch)
        );

        let mut wrong_state = context();
        wrong_state.durable_state_version = 2;
        assert_eq!(
            materialize_conflict_plan(&merge, &wrong_state),
            Err(MaterializationFailure::ClassificationEvidenceMismatch)
        );

        let mut forged_merge = context();
        forged_merge.merged_content = Some(fingerprint(9));
        assert_eq!(
            materialize_conflict_plan(&merge, &forged_merge),
            Err(MaterializationFailure::ClassificationEvidenceMismatch)
        );
    }

    #[test]
    fn materialization_carries_the_exact_classification_evidence_forward() {
        let classification = classify(ConflictCase::BothBinaryChanged);
        let plan = materialize_conflict_plan(&classification, &context()).expect("materialize");
        assert_eq!(
            plan.classification_evidence,
            classification.evidence().clone()
        );
    }

    #[test]
    fn durable_draft_reuses_r3_1_intent_fingerprint_contract() {
        let classification = classify(ConflictCase::RemoteContentChanged {
            guarded_local_replace: crate::conflict::GuardedLocalCapability::Available,
        });
        let exact_context = context_for(&classification);
        let plan = materialize_conflict_plan(&classification, &exact_context).expect("plan");
        let intent = mutation_intent_from_draft(&plan, &plan.drafts[0], 42)
            .expect("convert")
            .expect("durable intent");
        assert_eq!(intent.intent_fingerprint, intent.canonical_fingerprint());
        assert_eq!(intent.operation_kind, MutationOperationKind::LocalPublish);
        let mut forged = plan.drafts[0].clone();
        forged.operation_marker.push_str(".forged");
        assert_eq!(
            mutation_intent_from_draft(&plan, &forged, 42),
            Err(MaterializationFailure::DraftNotInPlan)
        );
    }

    #[test]
    fn emitted_local_intents_register_through_r3_1_store_validation() {
        let temp = tempfile::tempdir().expect("temp root");
        let root = temp.path().canonicalize().expect("canonical temp root");
        let app_data = root.join("private");
        let vault = root.join("vault");
        fs::create_dir(&app_data).expect("private root");
        fs::create_dir(&vault).expect("vault root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&app_data, fs::Permissions::from_mode(0o700))
                .expect("private permissions");
        }
        let mut store =
            crate::SyncStore::open(&app_data, &vault, Uuid::new_v4()).expect("sync store");
        let cases = [
            ConflictCase::RemoteContentChanged {
                guarded_local_replace: crate::conflict::GuardedLocalCapability::Available,
            },
            ConflictCase::NonOverlappingMarkdownChanges,
            ConflictCase::BothBinaryChanged,
        ];
        for (index, case) in cases.into_iter().enumerate() {
            let classification = classify(case);
            let exact_context = context_for(&classification);
            let plan = materialize_conflict_plan(&classification, &exact_context).expect("plan");
            for draft in &plan.drafts {
                let Some(intent) = mutation_intent_from_draft(&plan, draft, 100 + index as u64)
                    .expect("intent conversion")
                else {
                    continue;
                };
                if intent.operation_kind == MutationOperationKind::RemoteExistingBlocked {
                    continue;
                }
                assert_eq!(
                    store
                        .register_mutation_intent(&intent, None)
                        .expect("R3.1 registration"),
                    crate::MutationRegistrationOutcome::Registered
                );
            }
        }
    }

    #[test]
    fn materialized_intents_round_trip_through_sealed_replay_proof() {
        let cases = [
            (
                ConflictCase::RemoteContentChanged {
                    guarded_local_replace: crate::conflict::GuardedLocalCapability::Available,
                },
                LocalPlanKind::LocalPublish,
            ),
            (
                ConflictCase::BothBinaryChanged,
                LocalPlanKind::ConflictCopyPublish,
            ),
            (
                ConflictCase::NonOverlappingMarkdownChanges,
                LocalPlanKind::MergePublish,
            ),
            (
                ConflictCase::NonOverlappingMarkdownChanges,
                LocalPlanKind::BasePublish,
            ),
        ];
        for (index, (case, expected_kind)) in cases.into_iter().enumerate() {
            let classification = classify(case);
            let exact_context = context_for(&classification);
            let plan = materialize_conflict_plan(&classification, &exact_context).expect("plan");
            let draft = plan
                .drafts
                .iter()
                .find(|draft| draft.kind == expected_kind)
                .expect("round-trip draft");
            let intent = mutation_intent_from_draft(&plan, draft, 10 + index as u64)
                .expect("intent")
                .expect("durable intent");
            let mut verification = MutationVerificationEvidence {
                evidence_id: Uuid::from_u128(100 + index as u128),
                operation_id: intent.operation_id,
                attempt_number: 1,
                capture_phase: MutationEvidenceCapturePhase::PostVerify,
                disposition: MutationDisposition::VerifiedApplied,
                outcome_code: None,
                observed_account_id: intent.account_id.clone(),
                observed_remote_root_id: intent.remote_root_id.clone(),
                observed_remote_file_id: intent.remote_file_id.clone(),
                observed_parent_id: intent.destination_parent_id.clone(),
                observed_path: intent.destination_path.clone(),
                observed_local_revision: intent.expected_local_revision.clone(),
                observed_remote_revision: intent.expected_remote_revision.clone(),
                observed_sha256: intent.expected_local_sha256.clone(),
                observed_byte_length: intent.expected_local_byte_length,
                observed_operation_marker: Some(intent.operation_marker.clone()),
                forbidden_side_effect: false,
                verified_received_byte_offset: None,
                resume_reference: None,
                evidence_fingerprint: String::new(),
                captured_at_unix_ms: 20 + index as u64,
            };
            verification.evidence_fingerprint = verification.canonical_fingerprint();
            let state = MutationState {
                operation_id: intent.operation_id,
                phase: MutationPhase::Completed,
                attempt_number: 1,
                state_version: plan.durable_state_version,
                disposition: Some(MutationDisposition::VerifiedApplied),
                next_attempt_at_unix_ms: None,
                retry_mode: None,
                resume_reference: None,
                last_evidence_id: Some(verification.evidence_id),
                outcome_code: None,
                updated_at_unix_ms: 20 + index as u64,
            };
            let proof = ReplayProof::from_r3_1(&intent, &state, &verification)
                .expect("sealed replay proof");
            let mut facts = classification.evidence().facts().clone();
            proof.bind_facts(&mut facts);
            if matches!(
                expected_kind,
                LocalPlanKind::LocalPublish | LocalPlanKind::MergePublish
            ) {
                facts.local = proof.expected_local_content().cloned();
            } else if expected_kind == LocalPlanKind::BasePublish {
                facts.base = proof.expected_local_content().cloned();
            }
            let replay = classify_conflict(
                ConflictInput::new(
                    ClassificationEvidence::new(facts),
                    crate::conflict::BoundaryAssessment::Approved,
                    ReplayAssessment::VerifiedAppliedExactPostState(proof),
                    ConflictCase::LocalContentChanged,
                )
                .expect("exact replay input"),
            );
            assert_eq!(replay.cell, ConflictCell::C29);
            assert_eq!(
                replay.outcome,
                if expected_kind == LocalPlanKind::ConflictCopyPublish {
                    ConflictOutcome::NeedsReconcile
                } else {
                    ConflictOutcome::NoOpVerified
                }
            );
        }
    }
}
