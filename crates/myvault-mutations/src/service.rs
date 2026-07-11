use std::io;

use crate::revision::to_recovery;
use crate::{MutationError, OperationId, TrashOperation};
use myvault_core::{
    CoreError, ManifestDigest, PrepareManifestOutcome, PublishItemOutcome, StagePayloadOutcome,
    TrashArea, TrashId, TrashManifestV1, Vault, VaultPath, MAX_TRASH_PAYLOAD_BYTES,
};
use myvault_recovery::{
    CompleteOutcome, JournalEvidence, RecoveryJournal, RecoveryOperationKind, RenameMoveIntent,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrashExecutionOutcome {
    pub operation_id: OperationId,
    pub prepared: Option<PrepareManifestOutcome>,
    pub staged: StagePayloadOutcome,
    pub published: PublishItemOutcome,
    pub completion: CompleteOutcome,
}

pub struct MutationService<'service> {
    vault: &'service Vault,
    journal: &'service RecoveryJournal,
}

impl<'service> MutationService<'service> {
    #[must_use]
    pub fn new(vault: &'service Vault, journal: &'service RecoveryJournal) -> Self {
        Self { vault, journal }
    }

    /// Plans a file-only trash operation without writing the vault or journal.
    ///
    /// # Errors
    /// Returns an error for an invalid timestamp, internal/non-file source, or
    /// a source exceeding the bounded trash payload limit.
    pub fn plan_trash(
        &self,
        source: &VaultPath,
        trashed_at_unix_ms: i64,
    ) -> Result<TrashOperation, MutationError> {
        if trashed_at_unix_ms < 0 {
            return Err(MutationError::InvalidOperation(
                "trash timestamp must be nonnegative",
            ));
        }
        let revision = self.vault.revision(source, MAX_TRASH_PAYLOAD_BYTES)?;
        TrashOperation::new(
            OperationId::new(),
            TrashId::new(),
            source,
            revision,
            trashed_at_unix_ms,
        )
    }

    /// Executes a fresh operation or safely routes an existing journal ID to retry.
    ///
    /// # Errors
    /// Preserves core/recovery outcome-unknown errors without reclassification.
    pub fn execute_trash(
        &self,
        operation: &TrashOperation,
    ) -> Result<TrashExecutionOutcome, MutationError> {
        match self
            .journal
            .read_evidence(operation.operation_id().as_uuid())
        {
            Ok(evidence) => return self.retry_from_evidence(operation, evidence),
            Err(myvault_recovery::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let manifest = operation.rebuild_manifest()?;
        let digest = manifest.digest()?;
        let intent = build_intent(operation, &digest)?;
        let prepared = self
            .vault
            .trash_store()
            .prepare_staging_manifest(operation.trash_id(), &manifest)?;
        self.journal.publish(&intent)?;
        self.continue_trash(operation, &digest, &intent, Some(prepared))
    }

    /// Retries an operation only when its immutable journal evidence matches exactly.
    ///
    /// # Errors
    /// Returns a mismatch/unsupported error before mutating core state.
    pub fn retry_trash(
        &self,
        operation: &TrashOperation,
    ) -> Result<TrashExecutionOutcome, MutationError> {
        let evidence = self
            .journal
            .read_evidence(operation.operation_id().as_uuid())?;
        self.retry_from_evidence(operation, evidence)
    }

    /// Reconstructs and resumes one supported v3 trash intent.
    ///
    /// # Errors
    /// Unsupported evidence and every manifest/intent mismatch fail before mutation.
    pub fn resume_trash(
        &self,
        operation_id: OperationId,
    ) -> Result<TrashExecutionOutcome, MutationError> {
        let evidence = self.journal.read_evidence(operation_id.as_uuid())?;
        let intent = supported_trash_intent(evidence)?;
        let (trash_id, digest) = trash_binding(&intent)?;
        let manifest = self.read_unique_manifest(trash_id)?;
        let operation = TrashOperation::from_manifest(&manifest)?;
        if operation.operation_id() != operation_id {
            return Err(MutationError::IntentMismatch);
        }
        validate_intent(&operation, &digest, &intent)?;
        self.continue_trash(&operation, &digest, &intent, None)
    }

    fn retry_from_evidence(
        &self,
        operation: &TrashOperation,
        evidence: JournalEvidence,
    ) -> Result<TrashExecutionOutcome, MutationError> {
        let intent = supported_trash_intent(evidence)?;
        let manifest = operation.rebuild_manifest()?;
        let digest = manifest.digest()?;
        validate_intent(operation, &digest, &intent)?;
        self.continue_trash(operation, &digest, &intent, None)
    }

    fn continue_trash(
        &self,
        operation: &TrashOperation,
        digest: &ManifestDigest,
        intent: &RenameMoveIntent,
        prepared: Option<PrepareManifestOutcome>,
    ) -> Result<TrashExecutionOutcome, MutationError> {
        let source = operation.source_path()?;
        let staged = self.vault.trash_store().stage_payload_if_revision(
            operation.trash_id(),
            &source,
            digest,
        )?;
        let published = self
            .vault
            .trash_store()
            .publish_staging_item(operation.trash_id(), digest)?;
        let completion = self
            .journal
            .complete(operation.operation_id().as_uuid(), intent)?;
        Ok(TrashExecutionOutcome {
            operation_id: operation.operation_id(),
            prepared,
            staged,
            published,
            completion,
        })
    }

    fn read_unique_manifest(&self, trash_id: TrashId) -> Result<TrashManifestV1, MutationError> {
        let store = self.vault.trash_store();
        let items = store.read_manifest(TrashArea::Items, trash_id);
        let staging = store.read_manifest(TrashArea::Staging, trash_id);
        match (items, staging) {
            (Ok(_), Ok(_)) => Err(MutationError::InvalidOperation(
                "staging and items manifests both exist",
            )),
            (Ok(manifest), Err(error)) if is_not_found(&error) => Ok(manifest),
            (Err(error), Ok(manifest)) if is_not_found(&error) => Ok(manifest),
            (Err(items_error), Err(staging_error))
                if is_not_found(&items_error) && is_not_found(&staging_error) =>
            {
                Err(MutationError::InvalidOperation("trash manifest is missing"))
            }
            (Err(error), _) | (_, Err(error)) => Err(error.into()),
        }
    }
}

fn build_intent(
    operation: &TrashOperation,
    digest: &ManifestDigest,
) -> Result<RenameMoveIntent, MutationError> {
    Ok(RenameMoveIntent::new_trash(
        operation.operation_id().as_uuid(),
        operation.trash_id().as_uuid(),
        digest.as_str().to_owned(),
        operation.source(),
        to_recovery(operation.revision()),
    )?)
}

fn validate_intent(
    operation: &TrashOperation,
    digest: &ManifestDigest,
    observed: &RenameMoveIntent,
) -> Result<(), MutationError> {
    let expected = build_intent(operation, digest)?;
    if &expected == observed {
        Ok(())
    } else {
        Err(MutationError::IntentMismatch)
    }
}

fn supported_trash_intent(evidence: JournalEvidence) -> Result<RenameMoveIntent, MutationError> {
    match evidence {
        JournalEvidence::Supported(intent)
            if matches!(intent.kind, RecoveryOperationKind::Trash { .. }) =>
        {
            Ok(intent)
        }
        JournalEvidence::Supported(_) => Err(MutationError::InvalidOperation(
            "journal operation is not trash",
        )),
        JournalEvidence::Unsupported {
            operation_id,
            version,
        } => Err(MutationError::UnsupportedEvidence {
            operation_id,
            version,
        }),
    }
}

fn trash_binding(intent: &RenameMoveIntent) -> Result<(TrashId, ManifestDigest), MutationError> {
    let RecoveryOperationKind::Trash {
        trash_id,
        manifest_blake3,
    } = &intent.kind
    else {
        return Err(MutationError::InvalidOperation(
            "journal operation is not trash",
        ));
    };
    let trash_id = TrashId::parse(&trash_id.to_string())?;
    let digest = ManifestDigest::parse(manifest_blake3.clone())?;
    Ok((trash_id, digest))
}

fn is_not_found(error: &CoreError) -> bool {
    matches!(error, CoreError::Io(source) if source.kind() == io::ErrorKind::NotFound)
}
