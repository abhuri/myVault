use std::fmt;

use myvault_core::{FileRevision, TrashId, TrashManifestV1, VaultPath};
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

    pub(crate) fn from_manifest(manifest: &TrashManifestV1) -> Result<Self, MutationError> {
        let operation_id = OperationId::parse(&manifest.operation_id.to_string())?;
        let source = VaultPath::from_portable(&manifest.original_path)?;
        Self::new(
            operation_id,
            manifest.trash_id,
            &source,
            manifest.revision.clone(),
            manifest.trashed_at_unix_ms,
        )
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
