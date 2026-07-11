use std::io;

use crate::revision::{to_core, to_recovery};
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
        vault: &Vault,
        source: &VaultPath,
        trashed_at_unix_ms: i64,
    ) -> Result<TrashOperation, MutationError> {
        if trashed_at_unix_ms < 0 {
            return Err(MutationError::InvalidOperation(
                "trash timestamp must be nonnegative",
            ));
        }
        let revision = vault.revision(source, MAX_TRASH_PAYLOAD_BYTES)?;
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
        self.journal.publish(&intent)?;
        let prepared = self.ensure_manifest(operation, &digest)?;
        self.continue_trash(operation, &digest, &intent, prepared)
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

    /// Reconstructs and resumes one supported v4 trash intent.
    ///
    /// # Errors
    /// Unsupported evidence and every manifest/intent mismatch fail before mutation.
    pub fn resume_trash(
        &self,
        operation_id: OperationId,
    ) -> Result<TrashExecutionOutcome, MutationError> {
        let evidence = self.journal.read_evidence(operation_id.as_uuid())?;
        let intent = supported_trash_intent(evidence)?;
        let (operation, digest) = operation_from_intent(&intent)?;
        if operation.operation_id() != operation_id {
            return Err(MutationError::IntentMismatch);
        }
        validate_intent(&operation, &digest, &intent)?;
        let prepared = self.ensure_manifest(&operation, &digest)?;
        self.continue_trash(&operation, &digest, &intent, prepared)
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
        let prepared = self.ensure_manifest(operation, &digest)?;
        self.continue_trash(operation, &digest, &intent, prepared)
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

    fn ensure_manifest(
        &self,
        operation: &TrashOperation,
        digest: &ManifestDigest,
    ) -> Result<Option<PrepareManifestOutcome>, MutationError> {
        let expected = operation.rebuild_manifest()?;
        if expected.digest()? != *digest {
            return Err(MutationError::IntentMismatch);
        }
        match self.read_unique_manifest(operation.trash_id())? {
            Some(observed) => {
                if observed != expected || observed.digest()? != *digest {
                    return Err(MutationError::IntentMismatch);
                }
                Ok(None)
            }
            None => Ok(Some(
                self.vault
                    .trash_store()
                    .prepare_staging_manifest(operation.trash_id(), &expected)?,
            )),
        }
    }

    fn read_unique_manifest(
        &self,
        trash_id: TrashId,
    ) -> Result<Option<TrashManifestV1>, MutationError> {
        let store = self.vault.trash_store();
        let items = store.read_manifest(TrashArea::Items, trash_id);
        let staging = store.read_manifest(TrashArea::Staging, trash_id);
        match (items, staging) {
            (Ok(_), Ok(_)) => Err(MutationError::InvalidOperation(
                "staging and items manifests both exist",
            )),
            (Ok(manifest), Err(error)) if is_not_found(&error) => Ok(Some(manifest)),
            (Err(error), Ok(manifest)) if is_not_found(&error) => Ok(Some(manifest)),
            (Ok(_), Err(error)) | (Err(error), Ok(_)) => Err(error.into()),
            (Err(items_error), Err(staging_error)) => {
                if !is_not_found(&items_error) {
                    Err(items_error.into())
                } else if !is_not_found(&staging_error) {
                    Err(staging_error.into())
                } else {
                    Ok(None)
                }
            }
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
        operation.trashed_at_unix_ms(),
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

fn operation_from_intent(
    intent: &RenameMoveIntent,
) -> Result<(TrashOperation, ManifestDigest), MutationError> {
    let RecoveryOperationKind::Trash {
        trash_id,
        manifest_blake3,
        trashed_at_unix_ms,
    } = &intent.kind
    else {
        return Err(MutationError::InvalidOperation(
            "journal operation is not trash",
        ));
    };
    let trash_id = TrashId::parse(&trash_id.to_string())?;
    let digest = ManifestDigest::parse(manifest_blake3.clone())?;
    let operation_id = OperationId::parse(&intent.operation_id.to_string())?;
    let source = VaultPath::from_portable(&intent.from)?;
    let operation = TrashOperation::new(
        operation_id,
        trash_id,
        &source,
        to_core(&intent.expected)?,
        *trashed_at_unix_ms,
    )?;
    Ok((operation, digest))
}

fn is_not_found(error: &CoreError) -> bool {
    matches!(error, CoreError::Io(source) if source.kind() == io::ErrorKind::NotFound)
}
