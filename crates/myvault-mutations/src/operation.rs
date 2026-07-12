use std::fmt;

use myvault_core::{FileRevision, ManifestDigest, TrashId, TrashManifestV1, VaultPath};
use uuid::Uuid;

use crate::MutationError;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OperationId(Uuid);

impl OperationId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parses one canonical lowercase, hyphenated, nonnil UUID.
    ///
    /// # Errors
    /// Returns an error for aliases, malformed text, or the nil UUID.
    pub fn parse(value: &str) -> Result<Self, MutationError> {
        let id = Uuid::parse_str(value)
            .map_err(|_| MutationError::InvalidOperation("invalid operation id"))?;
        if id.is_nil() || id.to_string() != value {
            return Err(MutationError::InvalidOperation(
                "operation id must be canonical and nonnil",
            ));
        }
        Ok(Self(id))
    }

    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrashOperation {
    operation_id: OperationId,
    trash_id: TrashId,
    source: String,
    revision: FileRevision,
    trashed_at_unix_ms: i64,
}

impl TrashOperation {
    pub(crate) fn new(
        operation_id: OperationId,
        trash_id: TrashId,
        source: &VaultPath,
        revision: FileRevision,
        trashed_at_unix_ms: i64,
    ) -> Result<Self, MutationError> {
        let operation = Self {
            operation_id,
            trash_id,
            source: source.as_str().to_owned(),
            revision,
            trashed_at_unix_ms,
        };
        operation.rebuild_manifest()?;
        Ok(operation)
    }

    pub(crate) fn source_path(&self) -> Result<VaultPath, MutationError> {
        let path = VaultPath::from_portable(&self.source)?;
        if path.as_str() != self.source {
            return Err(MutationError::InvalidOperation(
                "trash source is not canonical",
            ));
        }
        Ok(path)
    }

    pub(crate) fn rebuild_manifest(&self) -> Result<TrashManifestV1, MutationError> {
        let source = self.source_path()?;
        Ok(TrashManifestV1::new(
            self.trash_id,
            self.operation_id.as_uuid(),
            &source,
            self.revision.clone(),
            self.trashed_at_unix_ms,
        )?)
    }

    #[must_use]
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    #[must_use]
    pub fn trash_id(&self) -> TrashId {
        self.trash_id
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub fn revision(&self) -> &FileRevision {
        &self.revision
    }

    #[must_use]
    pub fn trashed_at_unix_ms(&self) -> i64 {
        self.trashed_at_unix_ms
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreOperation {
    operation_id: OperationId,
    trash_id: TrashId,
    destination: String,
    revision: FileRevision,
    manifest_digest: String,
}

impl RestoreOperation {
    pub(crate) fn new(
        operation_id: OperationId,
        trash_id: TrashId,
        destination: &VaultPath,
        revision: FileRevision,
        manifest_digest: impl Into<String>,
    ) -> Result<Self, MutationError> {
        let manifest_digest = manifest_digest.into();
        ManifestDigest::parse(manifest_digest.clone())?;
        myvault_recovery::RenameMoveIntent::new_restore(
            operation_id.as_uuid(),
            trash_id.as_uuid(),
            manifest_digest.clone(),
            destination.as_str(),
            crate::revision::to_recovery(&revision),
        )?;
        Ok(Self {
            operation_id,
            trash_id,
            destination: destination.as_str().to_owned(),
            revision,
            manifest_digest,
        })
    }

    pub(crate) fn destination_path(&self) -> Result<VaultPath, MutationError> {
        let path = VaultPath::from_portable(&self.destination)?;
        if path.as_str() != self.destination {
            return Err(MutationError::InvalidOperation(
                "restore destination is not canonical",
            ));
        }
        Ok(path)
    }

    #[must_use]
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    #[must_use]
    pub fn trash_id(&self) -> TrashId {
        self.trash_id
    }

    #[must_use]
    pub fn destination(&self) -> &str {
        &self.destination
    }

    #[must_use]
    pub fn revision(&self) -> &FileRevision {
        &self.revision
    }

    #[must_use]
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalMoveOperation {
    operation_id: OperationId,
    source: String,
    destination: String,
    revision: FileRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaseRenameOperation {
    operation_id: OperationId,
    source: String,
    destination: String,
    temporary: String,
    revision: FileRevision,
}

impl CaseRenameOperation {
    pub(crate) fn new(
        operation_id: OperationId,
        source: &VaultPath,
        destination: &VaultPath,
        revision: FileRevision,
    ) -> Result<Self, MutationError> {
        let temporary = case_rename_temporary(operation_id, source, destination)?;
        myvault_recovery::RenameMoveIntent::new_case_rename(
            operation_id.as_uuid(),
            source.as_str(),
            destination.as_str(),
            crate::revision::to_recovery(&revision),
            temporary.as_str(),
        )?;
        Ok(Self {
            operation_id,
            source: source.as_str().to_owned(),
            destination: destination.as_str().to_owned(),
            temporary: temporary.as_str().to_owned(),
            revision,
        })
    }

    pub(crate) fn paths(&self) -> Result<(VaultPath, VaultPath, VaultPath), MutationError> {
        let source = VaultPath::from_portable(&self.source)?;
        let destination = VaultPath::from_portable(&self.destination)?;
        let temporary = case_rename_temporary(self.operation_id, &source, &destination)?;
        if source.as_str() != self.source
            || destination.as_str() != self.destination
            || temporary.as_str() != self.temporary
        {
            return Err(MutationError::InvalidOperation(
                "case rename paths are not canonical or deterministic",
            ));
        }
        Ok((source, destination, temporary))
    }

    #[must_use]
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub fn destination(&self) -> &str {
        &self.destination
    }

    #[must_use]
    pub fn temporary(&self) -> &str {
        &self.temporary
    }

    #[must_use]
    pub fn revision(&self) -> &FileRevision {
        &self.revision
    }
}

fn case_rename_temporary(
    operation_id: OperationId,
    source: &VaultPath,
    destination: &VaultPath,
) -> Result<VaultPath, MutationError> {
    let source_parent = source
        .as_str()
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent);
    let destination_parent = destination
        .as_str()
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent);
    if source_parent != destination_parent {
        return Err(MutationError::InvalidOperation(
            "case rename paths must have the same exact parent",
        ));
    }
    let name = format!(".mvcr-{}.tmp", operation_id.as_uuid().simple());
    let portable = if source_parent.is_empty() {
        name
    } else {
        format!("{source_parent}/{name}")
    };
    Ok(VaultPath::from_portable(portable)?)
}

impl NormalMoveOperation {
    pub(crate) fn new(
        operation_id: OperationId,
        source: &VaultPath,
        destination: &VaultPath,
        revision: FileRevision,
    ) -> Result<Self, MutationError> {
        myvault_recovery::RenameMoveIntent::new(
            operation_id.as_uuid(),
            source.as_str(),
            destination.as_str(),
            crate::revision::to_recovery(&revision),
        )?;
        Ok(Self {
            operation_id,
            source: source.as_str().to_owned(),
            destination: destination.as_str().to_owned(),
            revision,
        })
    }

    pub(crate) fn paths(&self) -> Result<(VaultPath, VaultPath), MutationError> {
        let source = VaultPath::from_portable(&self.source)?;
        let destination = VaultPath::from_portable(&self.destination)?;
        if source.as_str() != self.source || destination.as_str() != self.destination {
            return Err(MutationError::InvalidOperation(
                "normal move paths are not canonical",
            ));
        }
        Ok((source, destination))
    }

    #[must_use]
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub fn destination(&self) -> &str {
        &self.destination
    }

    #[must_use]
    pub fn revision(&self) -> &FileRevision {
        &self.revision
    }
}
