use std::io;

use crate::revision::{to_core, to_recovery};
use crate::{
    CaseRenameOperation, MutationError, NormalMoveOperation, OperationId, RestoreOperation,
    TrashOperation,
};
use myvault_core::{
    CaseRenameOutcome, ManifestDigest, MoveContentOutcome, PrepareManifestOutcome,
    PublishItemOutcome, RestoreItemOutcome, StagePayloadOutcome, TrashArea, TrashId, Vault,
    VaultPath, MAX_TRASH_PAYLOAD_BYTES,
};
use myvault_recovery::{
    CompleteOutcome, JournalEvidence, PublishOutcome, RecoveryJournal, RecoveryOperationKind,
    RenameMoveIntent,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrashExecutionOutcome {
    pub operation_id: OperationId,
    pub prepared: PrepareManifestOutcome,
    pub staged: StagePayloadOutcome,
    pub published: PublishItemOutcome,
    pub completion: CompleteOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestoreExecutionOutcome {
    pub operation_id: OperationId,
    pub restored: RestoreItemOutcome,
    pub completion: CompleteOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NormalMoveExecutionOutcome {
    pub operation_id: OperationId,
    pub moved: MoveContentOutcome,
    pub completion: CompleteOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaseRenameExecutionOutcome {
    pub operation_id: OperationId,
    pub renamed: CaseRenameOutcome,
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

    /// Plans an original-path restore without moving the payload or writing a journal.
    ///
    /// # Errors
    /// Returns an error for missing or invalid immutable item evidence.
    pub fn plan_restore(
        vault: &Vault,
        trash_id: TrashId,
    ) -> Result<RestoreOperation, MutationError> {
        let manifest = vault
            .trash_store()
            .read_manifest(TrashArea::Items, trash_id)?;
        let digest = manifest.digest()?;
        let destination = VaultPath::from_portable(&manifest.original_path)?;
        RestoreOperation::new(
            OperationId::new(),
            trash_id,
            &destination,
            manifest.revision,
            digest.as_str(),
        )
    }

    /// Plans a bounded file-only normal move without mutating vault or journal.
    ///
    /// # Errors
    /// Returns an error for internal paths, aliases, invalid destinations,
    /// non-files, or sources exceeding the bounded move limit.
    pub fn plan_normal_move(
        vault: &Vault,
        source: &VaultPath,
        destination: &VaultPath,
    ) -> Result<NormalMoveOperation, MutationError> {
        let operation_id = OperationId::new();
        RenameMoveIntent::new(
            operation_id.as_uuid(),
            source.as_str(),
            destination.as_str(),
            myvault_recovery::FileRevision::from_bytes(&[]),
        )?;
        let revision = vault.revision(source, MAX_TRASH_PAYLOAD_BYTES)?;
        NormalMoveOperation::new(operation_id, source, destination, revision)
    }

    /// Plans a bounded, file-only case rename without mutating vault or journal.
    ///
    /// # Errors
    /// Returns an error unless both paths are canonical collision aliases in the
    /// same exact parent and the source is a supported bounded content file.
    pub fn plan_case_rename(
        vault: &Vault,
        source: &VaultPath,
        destination: &VaultPath,
    ) -> Result<CaseRenameOperation, MutationError> {
        let operation_id = OperationId::new();
        // Validate the complete topology before reading source content. The
        // operation constructor repeats this check with the observed revision.
        CaseRenameOperation::new(
            operation_id,
            source,
            destination,
            myvault_core::FileRevision::from_bytes(&[]),
        )?;
        let revision = vault.single_link_content_revision(source, MAX_TRASH_PAYLOAD_BYTES)?;
        CaseRenameOperation::new(operation_id, source, destination, revision)
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
        prepared: PrepareManifestOutcome,
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
    ) -> Result<PrepareManifestOutcome, MutationError> {
        let expected = operation.rebuild_manifest()?;
        if expected.digest()? != *digest {
            return Err(MutationError::IntentMismatch);
        }
        Ok(self
            .vault
            .trash_store()
            .prepare_staging_manifest(operation.trash_id(), &expected)?)
    }

    /// Executes a fresh original-path restore or routes existing evidence to retry.
    ///
    /// # Errors
    /// Preserves core/recovery outcome-unknown errors without reclassification.
    pub fn execute_restore(
        &self,
        operation: &RestoreOperation,
    ) -> Result<RestoreExecutionOutcome, MutationError> {
        match self
            .journal
            .read_evidence(operation.operation_id().as_uuid())
        {
            Ok(evidence) => return self.retry_restore_from_evidence(operation, evidence),
            Err(myvault_recovery::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let intent = build_restore_intent(operation)?;
        self.journal.publish(&intent)?;
        self.continue_restore(operation, &intent)
    }

    /// Retries only exact supported journal evidence for a retained restore.
    ///
    /// # Errors
    /// Mismatched or unsupported evidence fails before core mutation.
    pub fn retry_restore(
        &self,
        operation: &RestoreOperation,
    ) -> Result<RestoreExecutionOutcome, MutationError> {
        let evidence = self
            .journal
            .read_evidence(operation.operation_id().as_uuid())?;
        self.retry_restore_from_evidence(operation, evidence)
    }

    /// Resumes one supported v4 restore from journal and immutable item evidence.
    ///
    /// # Errors
    /// Unsupported evidence and every manifest/intent mismatch fail before mutation.
    pub fn resume_restore(
        &self,
        operation_id: OperationId,
    ) -> Result<RestoreExecutionOutcome, MutationError> {
        let evidence = self.journal.read_evidence(operation_id.as_uuid())?;
        let intent = supported_restore_intent(evidence)?;
        let operation = restore_operation_from_intent(&intent)?;
        if operation.operation_id() != operation_id {
            return Err(MutationError::IntentMismatch);
        }
        validate_restore_intent(&operation, &intent)?;
        self.continue_restore(&operation, &intent)
    }

    fn retry_restore_from_evidence(
        &self,
        operation: &RestoreOperation,
        evidence: JournalEvidence,
    ) -> Result<RestoreExecutionOutcome, MutationError> {
        let intent = supported_restore_intent(evidence)?;
        validate_restore_intent(operation, &intent)?;
        self.continue_restore(operation, &intent)
    }

    fn continue_restore(
        &self,
        operation: &RestoreOperation,
        intent: &RenameMoveIntent,
    ) -> Result<RestoreExecutionOutcome, MutationError> {
        let destination = operation.destination_path()?;
        let digest = self.validate_restore_manifest(operation)?;
        let restored = self.vault.trash_store().restore_item_if_revision(
            operation.trash_id(),
            &destination,
            &digest,
        )?;
        let completion = self
            .journal
            .complete(operation.operation_id().as_uuid(), intent)?;
        Ok(RestoreExecutionOutcome {
            operation_id: operation.operation_id(),
            restored,
            completion,
        })
    }

    fn validate_restore_manifest(
        &self,
        operation: &RestoreOperation,
    ) -> Result<ManifestDigest, MutationError> {
        let manifest = self
            .vault
            .trash_store()
            .read_manifest(TrashArea::Items, operation.trash_id())?;
        let observed_digest = manifest.digest()?;
        let expected_digest = ManifestDigest::parse(operation.manifest_digest().to_owned())?;
        if manifest.trash_id != operation.trash_id()
            || manifest.original_path != operation.destination()
            || manifest.revision != *operation.revision()
            || observed_digest != expected_digest
        {
            return Err(MutationError::IntentMismatch);
        }
        Ok(expected_digest)
    }

    /// Executes a fresh normal move or routes existing evidence to retry.
    ///
    /// # Errors
    /// Preserves core/recovery outcome-unknown errors without reclassification.
    pub fn execute_normal_move(
        &self,
        operation: &NormalMoveOperation,
    ) -> Result<NormalMoveExecutionOutcome, MutationError> {
        match self
            .journal
            .read_evidence(operation.operation_id().as_uuid())
        {
            Ok(evidence) => return self.retry_normal_from_evidence(operation, evidence),
            Err(myvault_recovery::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let intent = build_normal_intent(operation)?;
        let published = self.journal.publish(&intent)?;
        self.continue_normal_move(
            operation,
            &intent,
            matches!(published, PublishOutcome::Published),
        )
    }

    /// Retries only exact supported journal evidence for a retained normal move.
    ///
    /// # Errors
    /// Mismatched or unsupported evidence fails before core mutation.
    pub fn retry_normal_move(
        &self,
        operation: &NormalMoveOperation,
    ) -> Result<NormalMoveExecutionOutcome, MutationError> {
        let evidence = self
            .journal
            .read_evidence(operation.operation_id().as_uuid())?;
        self.retry_normal_from_evidence(operation, evidence)
    }

    /// Resumes one supported v4 normal move from journal evidence alone.
    ///
    /// # Errors
    /// Unsupported, wrong-kind, or invalid evidence fails before mutation.
    pub fn resume_normal_move(
        &self,
        operation_id: OperationId,
    ) -> Result<NormalMoveExecutionOutcome, MutationError> {
        let evidence = self.journal.read_evidence(operation_id.as_uuid())?;
        let intent = supported_normal_intent(evidence)?;
        let operation = normal_operation_from_intent(&intent)?;
        if operation.operation_id() != operation_id {
            return Err(MutationError::IntentMismatch);
        }
        validate_normal_intent(&operation, &intent)?;
        self.continue_normal_move(&operation, &intent, false)
    }

    fn retry_normal_from_evidence(
        &self,
        operation: &NormalMoveOperation,
        evidence: JournalEvidence,
    ) -> Result<NormalMoveExecutionOutcome, MutationError> {
        let intent = supported_normal_intent(evidence)?;
        validate_normal_intent(operation, &intent)?;
        self.continue_normal_move(operation, &intent, false)
    }

    fn continue_normal_move(
        &self,
        operation: &NormalMoveOperation,
        intent: &RenameMoveIntent,
        allow_source_move: bool,
    ) -> Result<NormalMoveExecutionOutcome, MutationError> {
        let (source, destination) = operation.paths()?;
        let moved = if allow_source_move {
            self.vault
                .move_content_file_if_revision(&source, &destination, operation.revision())?
        } else {
            self.vault.resume_content_file_move_if_revision(
                &source,
                &destination,
                operation.revision(),
            )?
        };
        let completion = self
            .journal
            .complete(operation.operation_id().as_uuid(), intent)?;
        Ok(NormalMoveExecutionOutcome {
            operation_id: operation.operation_id(),
            moved,
            completion,
        })
    }

    /// Executes a fresh case rename or routes retained evidence to the
    /// fail-closed resume path.
    ///
    /// # Errors
    /// Preserves core/recovery outcome-unknown errors without reclassification.
    pub fn execute_case_rename(
        &self,
        operation: &CaseRenameOperation,
    ) -> Result<CaseRenameExecutionOutcome, MutationError> {
        match self
            .journal
            .read_evidence(operation.operation_id().as_uuid())
        {
            Ok(evidence) => return self.retry_case_rename_from_evidence(operation, evidence),
            Err(myvault_recovery::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let intent = build_case_rename_intent(operation)?;
        let published = self.journal.publish(&intent)?;
        self.continue_case_rename(
            operation,
            &intent,
            matches!(published, PublishOutcome::Published),
        )
    }

    /// Retries only exact supported journal evidence for a retained case rename.
    ///
    /// # Errors
    /// Mismatched or unsupported evidence fails before core mutation.
    pub fn retry_case_rename(
        &self,
        operation: &CaseRenameOperation,
    ) -> Result<CaseRenameExecutionOutcome, MutationError> {
        let evidence = self
            .journal
            .read_evidence(operation.operation_id().as_uuid())?;
        self.retry_case_rename_from_evidence(operation, evidence)
    }

    /// Resumes one supported v4 case rename from journal evidence alone.
    ///
    /// # Errors
    /// Unsupported, wrong-kind, nondeterministic-temp, or mismatched evidence
    /// fails before mutation.
    pub fn resume_case_rename(
        &self,
        operation_id: OperationId,
    ) -> Result<CaseRenameExecutionOutcome, MutationError> {
        let evidence = self.journal.read_evidence(operation_id.as_uuid())?;
        let intent = supported_case_rename_intent(evidence)?;
        let operation = case_rename_operation_from_intent(&intent)?;
        if operation.operation_id() != operation_id {
            return Err(MutationError::IntentMismatch);
        }
        validate_case_rename_intent(&operation, &intent)?;
        self.continue_case_rename(&operation, &intent, false)
    }

    fn retry_case_rename_from_evidence(
        &self,
        operation: &CaseRenameOperation,
        evidence: JournalEvidence,
    ) -> Result<CaseRenameExecutionOutcome, MutationError> {
        let intent = supported_case_rename_intent(evidence)?;
        validate_case_rename_intent(operation, &intent)?;
        self.continue_case_rename(operation, &intent, false)
    }

    fn continue_case_rename(
        &self,
        operation: &CaseRenameOperation,
        intent: &RenameMoveIntent,
        allow_source_rename: bool,
    ) -> Result<CaseRenameExecutionOutcome, MutationError> {
        let (source, destination, temporary) = operation.paths()?;
        let renamed = if allow_source_rename {
            self.vault.case_rename_content_file_if_revision(
                &source,
                &destination,
                &temporary,
                operation.revision(),
            )?
        } else {
            self.vault.resume_case_rename_content_file_if_revision(
                &source,
                &destination,
                &temporary,
                operation.revision(),
            )?
        };
        let completion = self
            .journal
            .complete(operation.operation_id().as_uuid(), intent)?;
        Ok(CaseRenameExecutionOutcome {
            operation_id: operation.operation_id(),
            renamed,
            completion,
        })
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

fn build_restore_intent(operation: &RestoreOperation) -> Result<RenameMoveIntent, MutationError> {
    Ok(RenameMoveIntent::new_restore(
        operation.operation_id().as_uuid(),
        operation.trash_id().as_uuid(),
        operation.manifest_digest().to_owned(),
        operation.destination(),
        to_recovery(operation.revision()),
    )?)
}

fn validate_restore_intent(
    operation: &RestoreOperation,
    observed: &RenameMoveIntent,
) -> Result<(), MutationError> {
    if build_restore_intent(operation)? == *observed {
        Ok(())
    } else {
        Err(MutationError::IntentMismatch)
    }
}

fn supported_restore_intent(evidence: JournalEvidence) -> Result<RenameMoveIntent, MutationError> {
    match evidence {
        JournalEvidence::Supported(intent)
            if matches!(intent.kind, RecoveryOperationKind::Restore { .. }) =>
        {
            Ok(intent)
        }
        JournalEvidence::Supported(_) => Err(MutationError::InvalidOperation(
            "journal operation is not restore",
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

fn restore_operation_from_intent(
    intent: &RenameMoveIntent,
) -> Result<RestoreOperation, MutationError> {
    let RecoveryOperationKind::Restore {
        trash_id,
        manifest_blake3,
    } = &intent.kind
    else {
        return Err(MutationError::InvalidOperation(
            "journal operation is not restore",
        ));
    };
    let operation_id = OperationId::parse(&intent.operation_id.to_string())?;
    let trash_id = TrashId::parse(&trash_id.to_string())?;
    let destination = VaultPath::from_portable(&intent.to)?;
    RestoreOperation::new(
        operation_id,
        trash_id,
        &destination,
        to_core(&intent.expected)?,
        manifest_blake3.clone(),
    )
}

fn build_normal_intent(operation: &NormalMoveOperation) -> Result<RenameMoveIntent, MutationError> {
    Ok(RenameMoveIntent::new(
        operation.operation_id().as_uuid(),
        operation.source(),
        operation.destination(),
        to_recovery(operation.revision()),
    )?)
}

fn validate_normal_intent(
    operation: &NormalMoveOperation,
    observed: &RenameMoveIntent,
) -> Result<(), MutationError> {
    if build_normal_intent(operation)? == *observed {
        Ok(())
    } else {
        Err(MutationError::IntentMismatch)
    }
}

fn supported_normal_intent(evidence: JournalEvidence) -> Result<RenameMoveIntent, MutationError> {
    match evidence {
        JournalEvidence::Supported(intent)
            if matches!(intent.kind, RecoveryOperationKind::NormalMove) =>
        {
            Ok(intent)
        }
        JournalEvidence::Supported(_) => Err(MutationError::InvalidOperation(
            "journal operation is not a normal move",
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

fn normal_operation_from_intent(
    intent: &RenameMoveIntent,
) -> Result<NormalMoveOperation, MutationError> {
    if !matches!(intent.kind, RecoveryOperationKind::NormalMove) {
        return Err(MutationError::InvalidOperation(
            "journal operation is not a normal move",
        ));
    }
    let operation_id = OperationId::parse(&intent.operation_id.to_string())?;
    let source = VaultPath::from_portable(&intent.from)?;
    let destination = VaultPath::from_portable(&intent.to)?;
    NormalMoveOperation::new(
        operation_id,
        &source,
        &destination,
        to_core(&intent.expected)?,
    )
}

fn build_case_rename_intent(
    operation: &CaseRenameOperation,
) -> Result<RenameMoveIntent, MutationError> {
    Ok(RenameMoveIntent::new_case_rename(
        operation.operation_id().as_uuid(),
        operation.source(),
        operation.destination(),
        to_recovery(operation.revision()),
        operation.temporary(),
    )?)
}

fn validate_case_rename_intent(
    operation: &CaseRenameOperation,
    observed: &RenameMoveIntent,
) -> Result<(), MutationError> {
    if build_case_rename_intent(operation)? == *observed {
        Ok(())
    } else {
        Err(MutationError::IntentMismatch)
    }
}

fn supported_case_rename_intent(
    evidence: JournalEvidence,
) -> Result<RenameMoveIntent, MutationError> {
    match evidence {
        JournalEvidence::Supported(intent)
            if matches!(intent.kind, RecoveryOperationKind::CaseRename) =>
        {
            Ok(intent)
        }
        JournalEvidence::Supported(_) => Err(MutationError::InvalidOperation(
            "journal operation is not a case rename",
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

fn case_rename_operation_from_intent(
    intent: &RenameMoveIntent,
) -> Result<CaseRenameOperation, MutationError> {
    if !matches!(intent.kind, RecoveryOperationKind::CaseRename) {
        return Err(MutationError::InvalidOperation(
            "journal operation is not a case rename",
        ));
    }
    let operation_id = OperationId::parse(&intent.operation_id.to_string())?;
    let source = VaultPath::from_portable(&intent.from)?;
    let destination = VaultPath::from_portable(&intent.to)?;
    let operation = CaseRenameOperation::new(
        operation_id,
        &source,
        &destination,
        to_core(&intent.expected)?,
    )?;
    if intent.temp.as_deref() != Some(operation.temporary()) {
        return Err(MutationError::IntentMismatch);
    }
    Ok(operation)
}
