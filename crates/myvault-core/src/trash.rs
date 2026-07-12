use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::path::VaultPathClass;
use crate::{CoreError, FileRevision, Result, Vault, VaultPath, MAX_TRASH_PAYLOAD_BYTES};

pub const MAX_TRASH_MANIFEST_BYTES: usize = 16 * 1024;
pub const MAX_TRASH_LIST_SCAN: usize = 8_192;
pub const MAX_TRASH_PAGE_SIZE: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrashArea {
    Staging,
    Items,
}

impl TrashArea {
    pub(crate) fn component(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Items => "items",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TrashId(Uuid);

impl TrashId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parses one canonical lowercase, hyphenated, nonnil UUID.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidTrashPath`] for every alias or nil UUID.
    pub fn parse(value: &str) -> Result<Self> {
        let id = Uuid::parse_str(value)
            .map_err(|_| CoreError::InvalidTrashPath(Path::new(value).to_owned()))?;
        if id.is_nil() || id.to_string() != value {
            return Err(CoreError::InvalidTrashPath(Path::new(value).to_owned()));
        }
        Ok(Self(id))
    }

    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for TrashId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TrashId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PayloadKind {
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestDigest(String);

impl ManifestDigest {
    /// Parses canonical lowercase BLAKE3 hex.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidTrashManifest`] for malformed input.
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if is_lower_blake3_hex(&value) {
            Ok(Self(value))
        } else {
            Err(CoreError::InvalidTrashManifest(
                "manifest digest must be lowercase BLAKE3 hex",
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrashManifestV1 {
    pub version: u32,
    pub trash_id: TrashId,
    pub operation_id: Uuid,
    pub original_path: String,
    pub payload_kind: PayloadKind,
    pub revision: FileRevision,
    pub trashed_at_unix_ms: i64,
}

impl TrashManifestV1 {
    /// Builds a canonical, semantically valid v1 file manifest.
    ///
    /// # Errors
    /// Returns an error for nil/mismatched IDs, internal/noncanonical paths,
    /// invalid or oversized revisions, and negative timestamps.
    pub fn new(
        trash_id: TrashId,
        operation_id: Uuid,
        original_path: &VaultPath,
        revision: FileRevision,
        trashed_at_unix_ms: i64,
    ) -> Result<Self> {
        let manifest = Self {
            version: 1,
            trash_id,
            operation_id,
            original_path: original_path.as_str().to_owned(),
            payload_kind: PayloadKind::File,
            revision,
            trashed_at_unix_ms,
        };
        manifest.validate(Some(trash_id))?;
        Ok(manifest)
    }

    /// Returns compact deterministic JSON in the declared field order.
    ///
    /// # Errors
    /// Returns an error if public fields were mutated into an invalid state.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate(None)?;
        let bytes = serde_json::to_vec(self)
            .map_err(|_| CoreError::InvalidTrashManifest("manifest serialization failed"))?;
        if bytes.len() > MAX_TRASH_MANIFEST_BYTES {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "trash manifest bytes",
                limit: MAX_TRASH_MANIFEST_BYTES,
            });
        }
        Ok(bytes)
    }

    /// # Errors
    /// Returns an error if this manifest is not semantically valid.
    pub fn digest(&self) -> Result<ManifestDigest> {
        Ok(ManifestDigest::from_bytes(&self.canonical_bytes()?))
    }

    pub(crate) fn expected_revision(&self) -> Result<FileRevision> {
        self.revision.validate()?;
        Ok(self.revision.clone())
    }

    pub(crate) fn validate(&self, expected_id: Option<TrashId>) -> Result<()> {
        if self.version != 1 {
            return Err(CoreError::InvalidTrashManifest(
                "unsupported manifest version",
            ));
        }
        if self.trash_id.0.is_nil() {
            return Err(CoreError::InvalidTrashManifest("invalid trash id"));
        }
        if expected_id.is_some_and(|expected| expected != self.trash_id) {
            return Err(CoreError::InvalidTrashManifest(
                "trash id does not match path",
            ));
        }
        if self.operation_id.is_nil() {
            return Err(CoreError::InvalidTrashManifest(
                "operation id must be nonnil",
            ));
        }
        let original = VaultPath::from_portable(&self.original_path)
            .map_err(|_| CoreError::InvalidTrashManifest("invalid original path"))?;
        if original.as_str() != self.original_path || original.classify() != VaultPathClass::Content
        {
            return Err(CoreError::InvalidTrashManifest(
                "original path must be canonical content",
            ));
        }
        if self.payload_kind != PayloadKind::File {
            return Err(CoreError::InvalidTrashManifest("unsupported payload kind"));
        }
        let revision = self.expected_revision()?;
        revision.validate()?;
        if revision.byte_len > u64::try_from(MAX_TRASH_PAYLOAD_BYTES).unwrap_or(u64::MAX) {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "trash payload bytes",
                limit: MAX_TRASH_PAYLOAD_BYTES,
            });
        }
        if self.trashed_at_unix_ms < 0 {
            return Err(CoreError::InvalidTrashManifest(
                "trash timestamp must be nonnegative",
            ));
        }
        Ok(())
    }
}

pub struct TrashStore<'vault> {
    pub(crate) vault: &'vault Vault,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrashListEvidence {
    Supported {
        trash_id: TrashId,
        manifest: TrashManifestV1,
        manifest_digest: ManifestDigest,
    },
    Opaque {
        trash_id: TrashId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrashListPage {
    pub entries: Vec<TrashListEvidence>,
    pub invalid_name_count: usize,
    pub next_after: Option<TrashId>,
    pub has_more: bool,
    pub scanned_entries: usize,
}

impl<'vault> TrashStore<'vault> {
    #[must_use]
    pub fn new(vault: &'vault Vault) -> Self {
        Self { vault }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareManifestOutcome {
    Prepared,
    AlreadyPrepared,
    AlreadyPublished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StagePayloadOutcome {
    Staged(crate::MoveDurability),
    AlreadyStaged(crate::MoveDurability),
    AlreadyPublished(crate::MoveDurability),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishItemOutcome {
    Published(crate::MoveDurability),
    AlreadyPublished(crate::MoveDurability),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreItemOutcome {
    Restored(crate::MoveDurability),
    AlreadyRestored(crate::MoveDurability),
}

pub(crate) fn item_directory_path(area: TrashArea, id: TrashId) -> Result<VaultPath> {
    VaultPath::from_portable(format!(".trash/v1/{}/{id}", area.component()))
}

pub(crate) fn manifest_path(area: TrashArea, id: TrashId) -> Result<VaultPath> {
    VaultPath::from_portable(format!(".trash/v1/{}/{id}/manifest.json", area.component()))
}

pub(crate) fn payload_path(area: TrashArea, id: TrashId) -> Result<VaultPath> {
    VaultPath::from_portable(format!(".trash/v1/{}/{id}/payload", area.component()))
}

fn is_lower_blake3_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
