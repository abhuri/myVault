#![forbid(unsafe_code)]

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use myvault_core::VaultPath;
use myvault_private_fs as private_fs;
use serde::{Deserialize, Serialize};
use std::fmt;
#[cfg(all(test, unix))]
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use uuid::Uuid;

const JOURNAL_DIRECTORY: &str = "operation-journal";
const COMPLETED_DIRECTORY: &str = "completed";
const MAX_ENTRY_BYTES: u64 = 64 * 1024;
const MAX_ENTRY_COUNT: usize = 4096;
/// Maximum physical children inspected in the journal directory, including
/// committed records, stale temps, junk, non-UTF-8 names, and `completed`.
/// This is deliberately separate from the logical active-evidence bound.
pub const MAX_DIRECTORY_ENTRY_COUNT: usize = 8192;
pub const MAX_PAGE_SIZE: usize = 128;

/// Recovery records are untrusted hints. They never authorize a vault mutation.
/// Callers must independently inspect the vault and apply their normal mutation policy.
pub const JOURNAL_IS_UNTRUSTED_HINT: bool = true;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidRoot(&'static str),
    PrivacyValidationRequired,
    ExtendedAcl,
    InvalidRevision,
    InvalidOperationId,
    InvalidTrashId,
    InvalidManifestDigest,
    InvalidOperationTopology,
    InvalidPortablePath,
    IdenticalPaths,
    CaseRenameContractRequired,
    InvalidCaseRenameContract,
    InvalidEntryName,
    EntryTooLarge,
    TooManyEntries,
    TooManyDirectoryEntries,
    InvalidPageSize,
    UnsupportedVersion(u32),
    JournalCollision,
    IntentMismatch,
    ExternalMutation,
    PublishedButNotSynced(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "invalid journal JSON: {error}"),
            Self::InvalidRoot(reason) => write!(formatter, "invalid recovery root: {reason}"),
            Self::PrivacyValidationRequired => formatter.write_str(
                "recovery journal disabled: robust platform privacy validation is required",
            ),
            Self::ExtendedAcl => formatter.write_str("recovery journal object has an extended ACL"),
            Self::InvalidRevision => formatter.write_str("invalid BLAKE3 revision"),
            Self::InvalidOperationId => formatter.write_str("invalid operation id"),
            Self::InvalidTrashId => formatter.write_str("invalid trash id"),
            Self::InvalidManifestDigest => {
                formatter.write_str("invalid canonical manifest BLAKE3 digest")
            }
            Self::InvalidOperationTopology => {
                formatter.write_str("operation kind does not match its endpoint topology")
            }
            Self::InvalidPortablePath => formatter.write_str("invalid portable vault path"),
            Self::IdenticalPaths => formatter.write_str("rename source and destination are equal"),
            Self::CaseRenameContractRequired => {
                formatter.write_str("case-only rename requires the explicit temp contract")
            }
            Self::InvalidCaseRenameContract => {
                formatter.write_str("invalid case-only rename temp contract")
            }
            Self::InvalidEntryName => formatter.write_str("invalid journal entry name"),
            Self::EntryTooLarge => formatter.write_str("journal entry exceeds size limit"),
            Self::TooManyEntries => formatter.write_str("journal contains too many entries"),
            Self::TooManyDirectoryEntries => formatter
                .write_str("journal directory contains too many physical entries (limit 8192)"),
            Self::InvalidPageSize => formatter.write_str("journal page size must be 1..=128"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported journal version {version}")
            }
            Self::JournalCollision => {
                formatter.write_str("operation id is already bound to a different intent")
            }
            Self::IntentMismatch => formatter.write_str("committed intent does not match expected"),
            Self::ExternalMutation => {
                formatter.write_str("journal topology was modified outside this operation")
            }
            Self::PublishedButNotSynced(error) => {
                write!(
                    formatter,
                    "journal published but directory sync failed: {error}"
                )
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryOperationKind {
    NormalMove,
    CaseRename,
    Trash {
        trash_id: Uuid,
        manifest_blake3: String,
        trashed_at_unix_ms: i64,
    },
    Restore {
        trash_id: Uuid,
        manifest_blake3: String,
    },
}

impl RecoveryOperationKind {
    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::NormalMove | Self::CaseRename => Ok(()),
            Self::Trash {
                trash_id,
                manifest_blake3,
                trashed_at_unix_ms,
            } => {
                if trash_id.is_nil() {
                    return Err(Error::InvalidTrashId);
                }
                if *trashed_at_unix_ms < 0 {
                    return Err(Error::InvalidOperationTopology);
                }
                validate_blake3_hex(manifest_blake3).map_err(|()| Error::InvalidManifestDigest)
            }
            Self::Restore {
                trash_id,
                manifest_blake3,
            } => {
                if trash_id.is_nil() {
                    return Err(Error::InvalidTrashId);
                }
                validate_blake3_hex(manifest_blake3).map_err(|()| Error::InvalidManifestDigest)
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<private_fs::Error> for Error {
    fn from(value: private_fs::Error) -> Self {
        match value {
            private_fs::Error::Io(error) => Self::Io(error),
            private_fs::Error::InvalidRoot(reason) => Self::InvalidRoot(reason),
            private_fs::Error::PrivacyValidationRequired => Self::PrivacyValidationRequired,
            private_fs::Error::ExtendedAcl => Self::ExtendedAcl,
            private_fs::Error::ExternalMutation => Self::ExternalMutation,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRevision {
    pub blake3_hex: String,
    pub byte_len: u64,
}

impl FileRevision {
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            blake3_hex: blake3::hash(bytes).to_hex().to_string(),
            byte_len: bytes.len() as u64,
        }
    }

    /// # Errors
    /// Returns [`Error::InvalidRevision`] unless the digest is canonical lowercase BLAKE3 hex.
    pub fn validate(&self) -> Result<(), Error> {
        validate_blake3_hex(&self.blake3_hex).map_err(|()| Error::InvalidRevision)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameMoveIntent {
    pub version: u32,
    pub operation_id: Uuid,
    pub from: String,
    pub to: String,
    pub expected: FileRevision,
    pub temp: Option<String>,
    pub kind: RecoveryOperationKind,
}

impl RenameMoveIntent {
    /// Version 4 makes a trash manifest durably reconstructable from journal
    /// evidence by binding its timestamp. Older records are rejected as
    /// unsupported rather than being silently reinterpreted.
    pub const VERSION: u32 = 4;

    /// Creates a normal rename/move intent. Case-only renames must use
    /// [`Self::new_case_rename`]. Input paths are stored canonically.
    ///
    /// # Errors
    /// Returns an error for invalid paths, equal paths, collision-key aliases, or revisions.
    pub fn new(
        operation_id: Uuid,
        from: impl AsRef<str>,
        to: impl AsRef<str>,
        expected: FileRevision,
    ) -> Result<Self, Error> {
        Self::new_for_kind(
            operation_id,
            RecoveryOperationKind::NormalMove,
            from,
            to,
            expected,
            None,
        )
    }

    /// Creates an explicit two-step case-only rename intent.
    ///
    /// # Errors
    /// Returns an error unless source/destination differ exactly, share a collision key,
    /// and the temporary path has a distinct collision key from both.
    pub fn new_case_rename(
        operation_id: Uuid,
        from: impl AsRef<str>,
        to: impl AsRef<str>,
        expected: FileRevision,
        temp: impl AsRef<str>,
    ) -> Result<Self, Error> {
        Self::new_for_kind(
            operation_id,
            RecoveryOperationKind::CaseRename,
            from,
            to,
            expected,
            Some(temp.as_ref().to_owned()),
        )
    }

    /// Creates a trash intent bound to an immutable trash manifest.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers, digest, paths, or revision.
    pub fn new_trash(
        operation_id: Uuid,
        trash_id: Uuid,
        manifest_blake3: impl Into<String>,
        trashed_at_unix_ms: i64,
        from: impl AsRef<str>,
        expected: FileRevision,
    ) -> Result<Self, Error> {
        let to = trash_payload_path("items", trash_id);
        let temp = trash_payload_path("staging", trash_id);
        Self::new_for_kind(
            operation_id,
            RecoveryOperationKind::Trash {
                trash_id,
                manifest_blake3: manifest_blake3.into(),
                trashed_at_unix_ms,
            },
            from,
            to,
            expected,
            Some(temp),
        )
    }

    /// Creates a restore intent bound to the same immutable trash manifest.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers, digest, paths, or revision.
    pub fn new_restore(
        operation_id: Uuid,
        trash_id: Uuid,
        manifest_blake3: impl Into<String>,
        to: impl AsRef<str>,
        expected: FileRevision,
    ) -> Result<Self, Error> {
        let from = trash_payload_path("items", trash_id);
        Self::new_for_kind(
            operation_id,
            RecoveryOperationKind::Restore {
                trash_id,
                manifest_blake3: manifest_blake3.into(),
            },
            from,
            to,
            expected,
            None,
        )
    }

    fn new_for_kind(
        operation_id: Uuid,
        kind: RecoveryOperationKind,
        from: impl AsRef<str>,
        to: impl AsRef<str>,
        expected: FileRevision,
        temp: Option<String>,
    ) -> Result<Self, Error> {
        if operation_id.is_nil() {
            return Err(Error::InvalidOperationId);
        }
        kind.validate()?;
        let from = canonical_portable(from.as_ref())?;
        let to = canonical_portable(to.as_ref())?;
        let temp = temp.map(|path| canonical_portable(&path)).transpose()?;
        validate_operation_topology(&kind, &from, &to, temp.as_deref())?;
        expected.validate()?;
        Ok(Self {
            version: Self::VERSION,
            operation_id,
            from,
            to,
            expected,
            temp,
            kind,
        })
    }

    fn validate(&self) -> Result<(), Error> {
        if self.version != Self::VERSION {
            return Err(Error::UnsupportedVersion(self.version));
        }
        if self.operation_id.is_nil() {
            return Err(Error::InvalidOperationId);
        }
        self.kind.validate()?;
        self.expected.validate()?;
        let from = canonical_portable(&self.from)?;
        let to = canonical_portable(&self.to)?;
        let temp = self.temp.as_deref().map(canonical_portable).transpose()?;
        if from != self.from || to != self.to || temp.as_deref() != self.temp.as_deref() {
            return Err(Error::InvalidPortablePath);
        }
        validate_operation_topology(&self.kind, &from, &to, temp.as_deref())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryDecision {
    NotStarted,
    InProgressAtTemp,
    Committed,
    DestinationCollision,
    DuplicateManual,
    DataLoss,
    ExternalMutation,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryTopology {
    pub from: Option<FileRevision>,
    pub to: Option<FileRevision>,
    pub temp: Option<FileRevision>,
}

/// Classifies observed topology conservatively. The journal is only an untrusted
/// hint; this function never mutates data or authorizes a mutation.
#[must_use]
pub fn decide_recovery(intent: &RenameMoveIntent, topology: &RecoveryTopology) -> RecoveryDecision {
    let expected = &intent.expected;
    match (&topology.from, &topology.to, &topology.temp) {
        (Some(from), None, None) if from == expected => RecoveryDecision::NotStarted,
        (None, None, Some(temp)) if temp == expected => RecoveryDecision::InProgressAtTemp,
        (None, Some(to), None) if to == expected => RecoveryDecision::Committed,
        (Some(from), Some(to), None) if from == expected && to == expected => {
            RecoveryDecision::DuplicateManual
        }
        (Some(from), Some(to), None) if from == expected && to != expected => {
            RecoveryDecision::DestinationCollision
        }
        (None, None, None) => RecoveryDecision::DataLoss,
        _ => RecoveryDecision::ExternalMutation,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Published,
    AlreadyPublished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompleteOutcome {
    Completed,
    AlreadyCompleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalPage {
    pub entries: Vec<RenameMoveIntent>,
    pub next_after: Option<Uuid>,
}

/// One immutable journal record classified without interpreting unsupported
/// schema bytes as the current intent type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalEvidence {
    Supported(RenameMoveIntent),
    Unsupported { operation_id: Uuid, version: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalEvidencePage {
    /// Entries ordered by canonical operation UUID. Unsupported entries consume
    /// one logical page slot and are never hidden by completion tombstones.
    pub entries: Vec<JournalEvidence>,
    pub next_after: Option<Uuid>,
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionTombstone {
    version: u32,
    operation_id: Uuid,
    intent_blake3_hex: String,
}

impl CompletionTombstone {
    const VERSION: u32 = 1;

    fn for_intent(intent: &RenameMoveIntent) -> Result<Self, Error> {
        Ok(Self {
            version: Self::VERSION,
            operation_id: intent.operation_id,
            intent_blake3_hex: blake3::hash(&canonical_bytes(intent)?).to_hex().to_string(),
        })
    }

    fn validate_for(&self, intent: &RenameMoveIntent) -> Result<(), Error> {
        let expected = Self::for_intent(intent)?;
        if self == &expected {
            Ok(())
        } else {
            Err(Error::IntentMismatch)
        }
    }
}

/// Append-only recovery evidence. Physical retention/garbage collection is
/// deliberately deferred; completion is represented only by tombstones.
pub struct RecoveryJournal {
    directory: Dir,
    completed: Dir,
}

impl RecoveryJournal {
    /// Opens a journal below a private app-data root. Both roots are opened
    /// without following symlinks and their identities are stabilized before
    /// the disjointness decision.
    ///
    /// # Errors
    /// Returns an error for unstable, symlinked, overlapping, or non-private roots.
    pub fn open(app_data_root: &Path, vault_root: &Path) -> Result<Self, Error> {
        let app_directory = private_fs::open_private_disjoint_root(app_data_root, vault_root)?;
        let directory = private_fs::create_or_open_private_dir(&app_directory, JOURNAL_DIRECTORY)?;
        let completed = private_fs::create_or_open_private_dir(&directory, COMPLETED_DIRECTORY)?;
        Ok(Self {
            directory,
            completed,
        })
    }

    /// Durably publishes an intent using a fresh temp and atomic no-replace rename.
    /// Stale temps are immutable crash evidence and are never removed or reused.
    ///
    /// # Errors
    /// Fails closed on collisions, unexpected topology, insecure files, or I/O errors.
    pub fn publish(&self, intent: &RenameMoveIntent) -> Result<PublishOutcome, Error> {
        self.publish_with_sync(intent, &mut sync_held_directory)
    }

    fn publish_with_sync<F>(
        &self,
        intent: &RenameMoveIntent,
        sync: &mut F,
    ) -> Result<PublishOutcome, Error>
    where
        F: FnMut(&Dir) -> Result<(), Error>,
    {
        intent.validate()?;
        let bytes = canonical_bytes(intent)?;
        let final_name = entry_name(intent.operation_id);
        if let Some(actual) = Self::read_raw_if_exists(&self.directory, &final_name)? {
            let outcome = compare_published(&actual, &bytes)?;
            sync(&self.directory).map_err(published_sync_error)?;
            return Ok(outcome);
        }
        if Self::publish_bytes(&self.directory, &final_name, &bytes, sync)? {
            Ok(PublishOutcome::Published)
        } else {
            Ok(PublishOutcome::AlreadyPublished)
        }
    }

    /// Publishes an immutable completion tombstone after verifying the original
    /// journal bytes. Neither the journal nor stale temp evidence is deleted.
    ///
    /// # Errors
    /// Fails on mismatched intent, unexpected temp files, insecure files, or I/O errors.
    pub fn complete(
        &self,
        operation_id: Uuid,
        expected_intent: &RenameMoveIntent,
    ) -> Result<CompleteOutcome, Error> {
        self.complete_with_sync(operation_id, expected_intent, &mut sync_held_directory)
    }

    fn complete_with_sync<F>(
        &self,
        operation_id: Uuid,
        expected_intent: &RenameMoveIntent,
        sync: &mut F,
    ) -> Result<CompleteOutcome, Error>
    where
        F: FnMut(&Dir) -> Result<(), Error>,
    {
        expected_intent.validate()?;
        if expected_intent.operation_id != operation_id {
            return Err(Error::IntentMismatch);
        }
        let final_name = entry_name(operation_id);
        let Some(actual) = Self::read_raw_if_exists(&self.directory, &final_name)? else {
            return Err(Error::IntentMismatch);
        };
        if actual != canonical_bytes(expected_intent)? {
            return Err(Error::IntentMismatch);
        }
        let tombstone = CompletionTombstone::for_intent(expected_intent)?;
        let tombstone_bytes = serde_json::to_vec(&tombstone)?;
        let tombstone_name = entry_name(operation_id);
        if let Some(actual) = Self::read_raw_if_exists(&self.completed, &tombstone_name)? {
            let observed: CompletionTombstone = serde_json::from_slice(&actual)?;
            observed.validate_for(expected_intent)?;
            if serde_json::to_vec(&observed)? != actual {
                return Err(Error::IntentMismatch);
            }
            sync(&self.completed).map_err(published_sync_error)?;
            return Ok(CompleteOutcome::AlreadyCompleted);
        }
        if Self::publish_bytes(&self.completed, &tombstone_name, &tombstone_bytes, sync)? {
            Ok(CompleteOutcome::Completed)
        } else {
            Ok(CompleteOutcome::AlreadyCompleted)
        }
    }

    /// Reads and validates one bounded, private journal entry.
    ///
    /// # Errors
    /// Returns an error for an absent, malformed, oversized, insecure, or mismatched entry.
    pub fn read(&self, operation_id: Uuid) -> Result<RenameMoveIntent, Error> {
        let bytes = Self::read_raw(&self.directory, &entry_name(operation_id))?;
        let intent = decode_intent(&bytes)?;
        intent.validate()?;
        if intent.operation_id != operation_id || canonical_bytes(&intent)? != bytes {
            return Err(Error::InvalidEntryName);
        }
        Ok(intent)
    }

    /// Reads one bounded journal entry while preserving unsupported schemas as
    /// opaque evidence. Unsupported bytes are not rewritten, deleted, or
    /// deserialized as the current intent type.
    ///
    /// # Errors
    /// Returns an error for malformed JSON, invalid current-schema evidence,
    /// insecure files, or an operation-id/filename mismatch.
    pub fn read_evidence(&self, operation_id: Uuid) -> Result<JournalEvidence, Error> {
        let bytes = Self::read_raw(&self.directory, &entry_name(operation_id))?;
        classify_evidence(operation_id, &bytes)
    }

    /// Lists a deterministic page of logically active entries. Only an exact,
    /// valid completion tombstone suppresses an entry. Journal records and stale
    /// temps are physically retained; bounded garbage collection is deferred.
    ///
    /// # Errors
    /// Returns an error for invalid limits, excessive committed entries, or invalid entries.
    pub fn list_page(&self, after: Option<Uuid>, limit: usize) -> Result<JournalPage, Error> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(Error::InvalidPageSize);
        }
        let ids = self.active_ids()?;
        let mut selected = ids
            .into_iter()
            .filter(|id| after.is_none_or(|cursor| *id > cursor))
            .take(limit + 1)
            .collect::<Vec<_>>();
        let has_more = selected.len() > limit;
        selected.truncate(limit);
        let next_after = if has_more {
            selected.last().copied()
        } else {
            None
        };
        let entries = selected
            .into_iter()
            .map(|id| self.read(id))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(JournalPage {
            entries,
            next_after,
        })
    }

    /// Lists a bounded deterministic UUID-ordered page containing both current
    /// v4 intents and opaque unsupported-version evidence. A cursor is the last
    /// returned UUID and the next page starts strictly after it.
    ///
    /// Valid completion tombstones suppress only their exact supported v4
    /// intent. Unsupported records remain visible because their tombstones
    /// cannot be authenticated without interpreting the unsupported schema.
    ///
    /// # Errors
    /// Returns an error for invalid limits, excessive active evidence, or any
    /// malformed/noncanonical current-schema record. Invalid evidence is never skipped.
    pub fn list_evidence_page(
        &self,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<JournalEvidencePage, Error> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(Error::InvalidPageSize);
        }
        let mut entries = Vec::new();
        for entry in self.directory_entries_bounded()? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(operation_id) = parse_entry_name(name) else {
                continue;
            };
            let evidence = self.read_evidence(operation_id)?;
            let active = match &evidence {
                JournalEvidence::Supported(intent) => !self.has_valid_completion_for(intent),
                JournalEvidence::Unsupported { .. } => true,
            };
            if active {
                entries.push((operation_id, evidence));
                if entries.len() > MAX_ENTRY_COUNT {
                    return Err(Error::TooManyEntries);
                }
            }
        }
        entries.sort_unstable_by_key(|(operation_id, _)| *operation_id);
        entries.dedup_by_key(|(operation_id, _)| *operation_id);
        let mut selected = entries
            .into_iter()
            .filter(|(operation_id, _)| after.is_none_or(|cursor| *operation_id > cursor))
            .take(limit + 1)
            .collect::<Vec<_>>();
        let has_more = selected.len() > limit;
        selected.truncate(limit);
        let next_after = if has_more {
            selected.last().map(|(operation_id, _)| *operation_id)
        } else {
            None
        };
        Ok(JournalEvidencePage {
            entries: selected.into_iter().map(|(_, evidence)| evidence).collect(),
            next_after,
        })
    }

    fn active_ids(&self) -> Result<Vec<Uuid>, Error> {
        let mut ids = Vec::new();
        for entry in self.directory_entries_bounded()? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = parse_entry_name(name) else {
                continue;
            };
            if self.has_valid_completion(id)? {
                continue;
            }
            ids.push(id);
            if ids.len() > MAX_ENTRY_COUNT {
                return Err(Error::TooManyEntries);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn directory_entries_bounded(&self) -> Result<Vec<cap_std::fs::DirEntry>, Error> {
        let mut entries = Vec::new();
        for entry in self.directory.entries()? {
            if entries.len() == MAX_DIRECTORY_ENTRY_COUNT {
                return Err(Error::TooManyDirectoryEntries);
            }
            entries.push(entry?);
        }
        Ok(entries)
    }

    fn has_valid_completion(&self, operation_id: Uuid) -> Result<bool, Error> {
        let intent = self.read(operation_id)?;
        Ok(self.has_valid_completion_for(&intent))
    }

    fn has_valid_completion_for(&self, intent: &RenameMoveIntent) -> bool {
        let operation_id = intent.operation_id;
        let name = entry_name(operation_id);
        let Ok(Some(bytes)) = Self::read_raw_if_exists(&self.completed, &name) else {
            return false;
        };
        let Ok(tombstone) = serde_json::from_slice::<CompletionTombstone>(&bytes) else {
            return false;
        };
        tombstone.validate_for(intent).is_ok()
            && serde_json::to_vec(&tombstone).is_ok_and(|canonical| canonical == bytes)
    }

    fn read_raw_if_exists(directory: &Dir, name: &str) -> Result<Option<Vec<u8>>, Error> {
        match Self::read_raw(directory, name) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn read_raw(directory: &Dir, name: &str) -> Result<Vec<u8>, Error> {
        let metadata = directory.symlink_metadata(name)?;
        if !metadata.file_type().is_file() {
            return Err(Error::InvalidEntryName);
        }
        if metadata.len() > MAX_ENTRY_BYTES {
            return Err(Error::EntryTooLarge);
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = directory.open_with(name, &options)?;
        private_fs::verify_private_file(&file, 1)?;
        let capacity = usize::try_from(metadata.len()).map_err(|_| Error::EntryTooLarge)?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Err(Error::EntryTooLarge);
        }
        Ok(bytes)
    }

    fn publish_bytes<F>(
        directory: &Dir,
        final_name: &str,
        bytes: &[u8],
        sync: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&Dir) -> Result<(), Error>,
    {
        let temporary_name = format!(".publish-{}.tmp", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = directory.open_with(&temporary_name, &options)?;
        private_fs::set_private_file_permissions(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        private_fs::verify_private_file(&file, 1)?;
        drop(file);
        match atomic_rename_noreplace(directory, &temporary_name, final_name) {
            Ok(()) => {
                sync(directory).map_err(published_sync_error)?;
                Ok(true)
            }
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                let actual = Self::read_raw(directory, final_name)?;
                if actual == bytes {
                    sync(directory).map_err(published_sync_error)?;
                    Ok(false)
                } else {
                    Err(Error::JournalCollision)
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn classify_evidence(operation_id: Uuid, bytes: &[u8]) -> Result<JournalEvidence, Error> {
    let envelope: RoutingEnvelope = serde_json::from_slice(bytes)?;
    if envelope.operation_id != operation_id {
        return Err(Error::InvalidEntryName);
    }
    if envelope.version != RenameMoveIntent::VERSION {
        return Ok(JournalEvidence::Unsupported {
            operation_id,
            version: envelope.version,
        });
    }
    let intent: RenameMoveIntent = serde_json::from_slice(bytes)?;
    intent.validate()?;
    if intent.operation_id != operation_id || canonical_bytes(&intent)? != bytes {
        return Err(Error::InvalidEntryName);
    }
    Ok(JournalEvidence::Supported(intent))
}

#[derive(Deserialize)]
struct RoutingEnvelope {
    version: u32,
    #[serde(deserialize_with = "deserialize_canonical_nonnil_uuid")]
    operation_id: Uuid,
}

fn deserialize_canonical_nonnil_uuid<'de, D>(deserializer: D) -> Result<Uuid, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    let operation_id = Uuid::parse_str(&text).map_err(serde::de::Error::custom)?;
    if operation_id.is_nil() || operation_id.to_string() != text {
        return Err(serde::de::Error::custom(
            "operation_id must be a canonical lowercase nonnil UUID",
        ));
    }
    Ok(operation_id)
}

fn canonical_portable(path: &str) -> Result<String, Error> {
    VaultPath::from_portable(path)
        .map(|path| path.as_str().to_owned())
        .map_err(|_| Error::InvalidPortablePath)
}

fn validate_blake3_hex(digest: &str) -> Result<(), ()> {
    let valid = digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    valid.then_some(()).ok_or(())
}

#[derive(Deserialize)]
struct VersionEnvelope {
    version: u32,
}

fn decode_intent(bytes: &[u8]) -> Result<RenameMoveIntent, Error> {
    let envelope: VersionEnvelope = serde_json::from_slice(bytes)?;
    if envelope.version != RenameMoveIntent::VERSION {
        return Err(Error::UnsupportedVersion(envelope.version));
    }
    Ok(serde_json::from_slice(bytes)?)
}

fn trash_payload_path(area: &str, trash_id: Uuid) -> String {
    format!(".trash/v1/{area}/{trash_id}/payload")
}

fn is_content_path(path: &str) -> Result<bool, Error> {
    let path = VaultPath::from_portable(path).map_err(|_| Error::InvalidPortablePath)?;
    let first_key = path
        .collision_key()
        .split('/')
        .next()
        .ok_or(Error::InvalidPortablePath)?
        .to_owned();
    Ok(first_key != ".trash" && first_key != ".obsidian")
}

fn require_content_path(path: &str) -> Result<(), Error> {
    if is_content_path(path)? {
        Ok(())
    } else {
        Err(Error::InvalidOperationTopology)
    }
}

fn validate_operation_topology(
    kind: &RecoveryOperationKind,
    from: &str,
    to: &str,
    temp: Option<&str>,
) -> Result<(), Error> {
    match kind {
        RecoveryOperationKind::NormalMove => {
            require_content_path(from)?;
            require_content_path(to)?;
            if temp.is_some() {
                return Err(Error::InvalidOperationTopology);
            }
            validate_path_relationship(from, to, false, None)
        }
        RecoveryOperationKind::CaseRename => {
            require_content_path(from)?;
            require_content_path(to)?;
            let Some(temp) = temp else {
                return Err(Error::InvalidOperationTopology);
            };
            // Exact reservation and ownership of this content path belong to
            // the mutation service. The journal is an untrusted hint and only
            // proves that the path is canonical, non-protected, and distinct.
            require_content_path(temp)?;
            validate_path_relationship(from, to, true, Some(temp))
        }
        RecoveryOperationKind::Trash { trash_id, .. } => {
            require_content_path(from)?;
            let expected_to = trash_payload_path("items", *trash_id);
            let expected_temp = trash_payload_path("staging", *trash_id);
            if to != expected_to || temp != Some(expected_temp.as_str()) {
                return Err(Error::InvalidOperationTopology);
            }
            Ok(())
        }
        RecoveryOperationKind::Restore { trash_id, .. } => {
            require_content_path(to)?;
            let expected_from = trash_payload_path("items", *trash_id);
            if from != expected_from || temp.is_some() {
                return Err(Error::InvalidOperationTopology);
            }
            Ok(())
        }
    }
}

fn validate_path_relationship(
    from: &str,
    to: &str,
    case_rename: bool,
    temp: Option<&str>,
) -> Result<(), Error> {
    if from == to {
        return Err(Error::IdenticalPaths);
    }
    let from_path = VaultPath::from_portable(from).map_err(|_| Error::InvalidPortablePath)?;
    let to_path = VaultPath::from_portable(to).map_err(|_| Error::InvalidPortablePath)?;
    let same_key = from_path.collision_key() == to_path.collision_key();
    if same_key && !case_rename {
        return Err(Error::CaseRenameContractRequired);
    }
    if case_rename {
        let Some(temp) = temp else {
            return Err(Error::InvalidCaseRenameContract);
        };
        let temp_path = VaultPath::from_portable(temp).map_err(|_| Error::InvalidPortablePath)?;
        if !same_key
            || temp_path.collision_key() == from_path.collision_key()
            || temp_path.collision_key() == to_path.collision_key()
        {
            return Err(Error::InvalidCaseRenameContract);
        }
    } else if let Some(temp) = temp {
        let temp_path = VaultPath::from_portable(temp).map_err(|_| Error::InvalidPortablePath)?;
        if temp_path.collision_key() == from_path.collision_key()
            || temp_path.collision_key() == to_path.collision_key()
        {
            return Err(Error::InvalidCaseRenameContract);
        }
    }
    Ok(())
}

fn canonical_bytes(intent: &RenameMoveIntent) -> Result<Vec<u8>, Error> {
    let bytes = serde_json::to_vec(intent)?;
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        return Err(Error::EntryTooLarge);
    }
    Ok(bytes)
}

fn compare_published(actual: &[u8], expected: &[u8]) -> Result<PublishOutcome, Error> {
    if actual == expected {
        Ok(PublishOutcome::AlreadyPublished)
    } else {
        Err(Error::JournalCollision)
    }
}

fn entry_name(operation_id: Uuid) -> String {
    format!("{operation_id}.json")
}

fn parse_entry_name(name: &str) -> Option<Uuid> {
    let id_text = name.strip_suffix(".json")?;
    let id = Uuid::parse_str(id_text).ok()?;
    (id.to_string() == id_text).then_some(id)
}

fn published_sync_error(error: Error) -> Error {
    match error {
        Error::Io(error) => Error::PublishedButNotSynced(error),
        other => other,
    }
}

fn sync_held_directory(directory: &Dir) -> Result<(), Error> {
    private_fs::sync_directory(directory).map_err(Error::from)
}

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
fn atomic_rename_noreplace(directory: &Dir, source: &str, destination: &str) -> Result<(), Error> {
    let held = directory.try_clone()?.into_std_file();
    rustix::fs::renameat_with(
        &held,
        source,
        &held,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| Error::Io(io::Error::from(error)))
}

#[cfg(not(any(target_os = "android", target_os = "linux", target_os = "macos")))]
fn atomic_rename_noreplace(
    _directory: &Dir,
    _source: &str,
    _destination: &str,
) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(all(test, unix))]
mod durability_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (tempfile::TempDir, RecoveryJournal) {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let base = temporary
            .path()
            .canonicalize()
            .expect("canonical temp root");
        let app = base.join("app");
        let vault = base.join("vault");
        fs::create_dir(&app).expect("app directory");
        fs::create_dir(&vault).expect("vault directory");
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app root");
        let journal = RecoveryJournal::open(&app, &vault).expect("open journal");
        (temporary, journal)
    }

    fn intent() -> RenameMoveIntent {
        RenameMoveIntent::new(
            Uuid::new_v4(),
            "source.md",
            "destination.md",
            FileRevision::from_bytes(b"note"),
        )
        .expect("intent")
    }

    #[test]
    fn identical_publish_retry_repeats_directory_sync_after_unknown_durability() {
        let (_temporary, journal) = fixture();
        let expected = intent();
        let mut sync_attempts = 0_usize;
        let mut fail_first_sync = |directory: &Dir| {
            sync_attempts += 1;
            if sync_attempts == 1 {
                Err(Error::Io(io::Error::other(
                    "injected directory sync failure",
                )))
            } else {
                sync_held_directory(directory)
            }
        };

        assert!(matches!(
            journal.publish_with_sync(&expected, &mut fail_first_sync),
            Err(Error::PublishedButNotSynced(_))
        ));
        assert_eq!(
            journal
                .publish_with_sync(&expected, &mut fail_first_sync)
                .expect("retry sync"),
            PublishOutcome::AlreadyPublished
        );
        assert_eq!(sync_attempts, 2);
    }

    #[test]
    fn identical_tombstone_retry_repeats_completed_directory_sync() {
        let (_temporary, journal) = fixture();
        let expected = intent();
        journal.publish(&expected).expect("publish intent");
        let mut sync_attempts = 0_usize;
        let mut fail_first_sync = |directory: &Dir| {
            sync_attempts += 1;
            if sync_attempts == 1 {
                Err(Error::Io(io::Error::other(
                    "injected tombstone sync failure",
                )))
            } else {
                sync_held_directory(directory)
            }
        };

        assert!(matches!(
            journal.complete_with_sync(expected.operation_id, &expected, &mut fail_first_sync,),
            Err(Error::PublishedButNotSynced(_))
        ));
        assert_eq!(
            journal
                .complete_with_sync(expected.operation_id, &expected, &mut fail_first_sync,)
                .expect("retry tombstone sync"),
            CompleteOutcome::AlreadyCompleted
        );
        assert_eq!(sync_attempts, 2);
    }
}
