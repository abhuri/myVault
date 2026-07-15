//! Safety-focused primitives for accessing a local Markdown vault.
//!
//! This crate deliberately contains no UI or platform integration. Callers must
//! keep authentication tokens and other secrets outside the derived index.

mod atomic_move;
mod capability;
mod error;
mod index;
mod path;
mod revision;
mod transfer;
mod trash;
mod vault;
mod watcher;

pub use error::{CoreError, Result};
pub use index::{DerivedIndex, NoteRecord, SCHEMA_VERSION, SQLITE_OPEN_RESIDUAL_RISK};
pub use path::VaultPath;
pub use revision::FileRevision;
pub use transfer::{stream_content_snapshot, ContentPublishOutcome, ContentSnapshot, Sha256Digest};
pub use trash::{
    ManifestDigest, PayloadKind, PrepareManifestOutcome, PublishItemOutcome, RestoreItemOutcome,
    StagePayloadOutcome, TrashArea, TrashId, TrashListEvidence, TrashListPage, TrashManifestV1,
    TrashStore, MAX_TRASH_LIST_SCAN, MAX_TRASH_MANIFEST_BYTES, MAX_TRASH_PAGE_SIZE,
};
pub use vault::{
    CaseRenameOutcome, CaseRenamePhase, DirectorySyncStatus, InventoryEntry, InventoryKind,
    InventoryLimits, MoveContentOutcome, MoveDurability, ReadNote, ReplaceContentOutcome, Vault,
    WriteIntent, DEFAULT_READ_LIMIT, MAX_NOTE_BYTES, MAX_TRASH_PAYLOAD_BYTES,
    MUTATION_EXTERNAL_PROCESS_RESIDUAL_RISK, TRASH_REVISION_EXTERNAL_PROCESS_RESIDUAL_RISK,
};
pub use watcher::{
    BurstNormalizer, NormalizedEvent, RawEvent, SelfWriteSuppressor, WriteFingerprint,
};
