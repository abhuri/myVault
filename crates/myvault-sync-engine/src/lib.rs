#![forbid(unsafe_code)]

//! Crash-safe production sync state and orchestration contracts.
//!
//! This crate performs no OAuth flow and contains no concrete network client.
//! Native adapters provide typed, already-authorized Drive pages while secrets
//! stay outside this boundary.

use myvault_core::VaultPath;
use serde::{Deserialize, Serialize};
use std::{fmt, io};
use uuid::Uuid;

pub mod conflict;
pub mod local_identity;
pub mod local_orchestration;
mod store;
mod sync_journal;

pub use local_orchestration::{
    classify_authoritative_final_outcome, handle_local_execution_echo_hint,
    AuthoritativeFinalOutcome, EchoHintDisposition, LocalExecutionEchoHint,
    LocalExecutionEchoSource, PlatformCallFact,
};
pub use store::{
    BindOutcome, ChangeBatch, ChangeBatchDependency, ChangeBatchDependencyKind, ConflictEvidence,
    ConflictEvidenceRegistrationOutcome, EnqueueOutcome, JobState, LocalExecutionAttemptBoundary,
    LocalExecutionAttemptOutcome, LocalExecutionContractRecord, LocalExecutionOutcome,
    LocalExecutionOutcomeRecord, LocalExecutionRecoveryObservation,
    LocalExecutionRegistrationOutcome, LocalExecutionWitnessPublicationOutcome, LocalMutationState,
    LocalMutationStatus, MutationDisposition, MutationEvent, MutationEvidenceCapturePhase,
    MutationIntent, MutationOperationKind, MutationOutcomeTransition, MutationPhase,
    MutationRegistrationOutcome, MutationRetryMode, MutationState, MutationVerificationEvidence,
    QueueJob, QueueJobKind, RemoteBaseEvidence, RemoteExistingBlockedInput, RemotePreviewCursor,
    RemotePreviewEntry, RemotePreviewPage, SyncStore, TransferCompletion,
    TransferCompletionOutcome, TransferDirection, TransferMimeClass, TransferPhase, TransferRecord,
    TransferRegistrationOutcome, TransferSummary, UntrustedLocalExecutionOutcomeClaim,
    VaultSyncState, MAX_REMOTE_PREVIEW_PAGE_SIZE, SCHEMA_VERSION, SQLITE_OPEN_RESIDUAL_RISK,
};

pub type Result<T> = std::result::Result<T, Error>;

pub const MAX_SCAN_PAGE_ENTRIES: usize = 1_000;
pub const MAX_SCAN_FRONTIER_FOLDERS: usize = 5_000;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    PrivateStorage(myvault_private_fs::Error),
    Database(rusqlite::Error),
    InvalidVaultId,
    InvalidRemoteId,
    InvalidRemoteToken,
    InvalidRemoteEntry,
    InvalidPortablePath,
    InvalidRevision,
    InvalidTimestamp,
    InvalidErrorCode,
    SyncLeaseHeld,
    BindingCollision,
    BindingIdentityMismatch,
    BindingRequiresAccount,
    InvalidStateTransition,
    InvalidSchema,
    UnsupportedSchema(i64),
    QueueCollision,
    JobNotFound,
    InvalidTransferEvidence,
    TransferCollision,
    TransferNotFound,
    CursorMismatch,
    RescanRequired,
    InvalidPreviewCursor,
    InvalidPreviewLimit,
    BatchAlreadyActive,
    NoActiveBatch,
    UnknownMutation,
    LocalMutationIncomplete,
    MutationNeedsReconcile,
    MutationNotFound,
    MutationCollision,
    LocalExecutionCollision,
    LocalExecutionNotFound,
    InvalidLocalExecutionEvidence,
    LocalExecutionJournalCollision,
    LocalExecutionJournalMalformed,
    LocalExecutionJournalMismatch,
    LocalExecutionJournalPublishedButNotSynced(io::Error),
    MutationStateVersionMismatch,
    UnsupportedTransferChange,
    TransferChangeMismatch,
    Remote(RemoteError),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("sync state I/O failed"),
            Self::PrivateStorage(_) => formatter.write_str("private sync storage is unavailable"),
            Self::Database(_) => formatter.write_str("sync database operation failed"),
            Self::InvalidVaultId => formatter.write_str("the local vault identifier is invalid"),
            Self::InvalidRemoteId => formatter.write_str("a remote identifier is invalid"),
            Self::InvalidRemoteToken => formatter.write_str("a remote page token is invalid"),
            Self::InvalidRemoteEntry => formatter.write_str("remote metadata is invalid"),
            Self::InvalidPortablePath => formatter.write_str("the portable sync path is invalid"),
            Self::InvalidRevision => formatter.write_str("the local revision is invalid"),
            Self::InvalidTimestamp => formatter.write_str("the sync timestamp is invalid"),
            Self::InvalidErrorCode => formatter.write_str("the redacted error code is invalid"),
            Self::SyncLeaseHeld => {
                formatter.write_str("another sync worker already holds this vault lease")
            }
            Self::BindingCollision => {
                formatter.write_str("the vault is already bound to a different remote root")
            }
            Self::BindingIdentityMismatch => formatter
                .write_str("the requested account and root do not match verified remote identity"),
            Self::BindingRequiresAccount => formatter
                .write_str("the migrated remote binding requires verified account identity"),
            Self::InvalidStateTransition => {
                formatter.write_str("the requested sync state transition is invalid")
            }
            Self::InvalidSchema => formatter.write_str("the sync database schema is invalid"),
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "sync database schema version {version} is unsupported"
                )
            }
            Self::QueueCollision => {
                formatter.write_str("the queue operation identifier has conflicting content")
            }
            Self::JobNotFound => formatter.write_str("the sync queue job was not found"),
            Self::InvalidTransferEvidence => {
                formatter.write_str("the durable transfer evidence is invalid")
            }
            Self::TransferCollision => {
                formatter.write_str("the transfer operation identifier has conflicting evidence")
            }
            Self::TransferNotFound => formatter.write_str("the durable transfer was not found"),
            Self::CursorMismatch => formatter.write_str("the durable cursor changed unexpectedly"),
            Self::RescanRequired => formatter.write_str("the remote cursor requires a full rescan"),
            Self::InvalidPreviewCursor => {
                formatter.write_str("the remote preview cursor is invalid")
            }
            Self::InvalidPreviewLimit => {
                formatter.write_str("the remote preview page size is invalid")
            }
            Self::BatchAlreadyActive => {
                formatter.write_str("an incremental change batch is already active")
            }
            Self::NoActiveBatch => formatter.write_str("no incremental change batch is active"),
            Self::UnknownMutation => {
                formatter.write_str("the local mutation was not declared by the active batch")
            }
            Self::LocalMutationIncomplete => {
                formatter.write_str("not all declared local mutations are committed")
            }
            Self::MutationNeedsReconcile => formatter
                .write_str("a local mutation has an unknown outcome and needs reconciliation"),
            Self::MutationNotFound => formatter.write_str("the durable mutation was not found"),
            Self::MutationCollision => formatter
                .write_str("the durable mutation identifier has conflicting immutable evidence"),
            Self::LocalExecutionCollision => formatter
                .write_str("the local execution identifier has conflicting immutable evidence"),
            Self::LocalExecutionNotFound => {
                formatter.write_str("the local execution contract was not found")
            }
            Self::InvalidLocalExecutionEvidence => {
                formatter.write_str("the local execution evidence is invalid")
            }
            Self::LocalExecutionJournalCollision => formatter
                .write_str("the local execution journal name has conflicting immutable evidence"),
            Self::LocalExecutionJournalMalformed => {
                formatter.write_str("the local execution journal evidence is invalid")
            }
            Self::LocalExecutionJournalMismatch => formatter
                .write_str("the local execution journal does not match the fresh durable binding"),
            Self::LocalExecutionJournalPublishedButNotSynced(_) => formatter.write_str(
                "local execution journal evidence was published but directory sync failed",
            ),
            Self::MutationStateVersionMismatch => {
                formatter.write_str("the durable mutation state version changed unexpectedly")
            }
            Self::UnsupportedTransferChange => {
                formatter.write_str("the remote change requires an unsupported local mutation")
            }
            Self::TransferChangeMismatch => formatter
                .write_str("the transfer does not exactly match its resolved remote change"),
            Self::Remote(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::LocalExecutionJournalPublishedButNotSynced(error) => {
                Some(error)
            }
            Self::PrivateStorage(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::Remote(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<myvault_private_fs::Error> for Error {
    fn from(error: myvault_private_fs::Error) -> Self {
        Self::PrivateStorage(error)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteError {
    code: String,
}

impl RemoteError {
    /// Creates a redacted remote error containing only a stable bounded code.
    ///
    /// # Errors
    /// Rejects empty, oversized, or non-portable codes.
    pub fn new(code: impl Into<String>) -> Result<Self> {
        let code = code.into();
        validate_redacted_code(&code)?;
        Ok(Self { code })
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "remote sync request failed ({})", self.code)
    }
}

impl std::error::Error for RemoteError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEntryKind {
    File,
    Folder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteHashAlgorithm {
    Md5,
    Sha1,
    Sha256,
}

impl RemoteHashAlgorithm {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    const fn expected_hex_len(self) -> usize {
        match self {
            Self::Md5 => 32,
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteContentHash {
    pub algorithm: RemoteHashAlgorithm,
    pub hex: String,
}

impl RemoteContentHash {
    /// Creates a typed, canonical lowercase remote checksum.
    ///
    /// # Errors
    /// Rejects an incorrect digest length or non-lowercase hexadecimal value.
    pub fn new(algorithm: RemoteHashAlgorithm, hex: impl Into<String>) -> Result<Self> {
        let value = Self {
            algorithm,
            hex: hex.into(),
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.hex.len() != self.algorithm.expected_hex_len()
            || !self
                .hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::InvalidRemoteEntry);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteEntry {
    pub file_id: String,
    pub parent_id: String,
    pub path: String,
    pub kind: RemoteEntryKind,
    pub content_hash: Option<RemoteContentHash>,
    pub remote_revision: String,
}

impl RemoteEntry {
    /// Validates bounded identifiers, a canonical content path, and revision metadata.
    ///
    /// # Errors
    /// Rejects protected paths, empty identifiers, and malformed hashes/revisions.
    pub fn validate(&self) -> Result<()> {
        validate_remote_id(&self.file_id)?;
        validate_remote_id(&self.parent_id)?;
        validate_content_path(&self.path)?;
        validate_remote_token(&self.remote_revision)?;
        if let Some(hash) = &self.content_hash {
            hash.validate()?;
        }
        if self.kind == RemoteEntryKind::Folder && self.content_hash.is_some() {
            return Err(Error::InvalidRemoteEntry);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanPage {
    pub entries: Vec<RemoteEntry>,
    pub next_page_token: Option<String>,
}

impl ScanPage {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.entries.len() > MAX_SCAN_PAGE_ENTRIES {
            return Err(Error::InvalidRemoteEntry);
        }
        validate_entries(&self.entries)?;
        if let Some(token) = &self.next_page_token {
            validate_remote_token(token)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteChange {
    Upsert(RemoteEntry),
    Removed { file_id: String },
}

impl RemoteChange {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Upsert(entry) => entry.validate(),
            Self::Removed { file_id } => validate_remote_id(file_id),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangesPage {
    pub changes: Vec<RemoteChange>,
    pub next_page_token: Option<String>,
    pub new_start_page_token: Option<String>,
}

/// Exact account/root identity proven by a native provider before persistence.
///
/// The requested values and the values observed from the provider must match,
/// which makes a folder name alone insufficient to create a binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedRemoteBinding {
    account_id: String,
    remote_root_id: String,
}

impl VerifiedRemoteBinding {
    /// Creates one exact verified binding.
    ///
    /// # Errors
    /// Rejects malformed IDs or any requested/observed identity mismatch.
    pub fn new(
        requested_account_id: impl Into<String>,
        requested_remote_root_id: impl Into<String>,
        observed_account_id: impl Into<String>,
        observed_remote_root_id: impl Into<String>,
    ) -> Result<Self> {
        let requested_account_id = requested_account_id.into();
        let requested_remote_root_id = requested_remote_root_id.into();
        let observed_account_id = observed_account_id.into();
        let observed_remote_root_id = observed_remote_root_id.into();
        validate_remote_id(&requested_account_id)?;
        validate_remote_id(&requested_remote_root_id)?;
        validate_remote_id(&observed_account_id)?;
        validate_remote_id(&observed_remote_root_id)?;
        if requested_account_id != observed_account_id
            || requested_remote_root_id != observed_remote_root_id
        {
            return Err(Error::BindingIdentityMismatch);
        }
        Ok(Self {
            account_id: requested_account_id,
            remote_root_id: requested_remote_root_id,
        })
    }

    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    #[must_use]
    pub fn remote_root_id(&self) -> &str {
        &self.remote_root_id
    }
}

/// One durable, bounded request for a direct-child page of a remote folder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanRequest {
    pub folder_id: String,
    /// Empty only for the bound root; descendants use canonical portable paths.
    pub folder_path: String,
    pub page_token: Option<String>,
}

impl ChangesPage {
    pub(crate) fn validate(&self) -> Result<()> {
        for change in &self.changes {
            change.validate()?;
        }
        match (&self.next_page_token, &self.new_start_page_token) {
            (Some(next), None) => validate_remote_token(next),
            (None, Some(start)) => validate_remote_token(start),
            _ => Err(Error::InvalidRemoteEntry),
        }
    }
}

/// Native production adapters implement this trait without exposing credentials.
pub trait DriveClient {
    /// Captures the Changes token that must precede an initial scan.
    ///
    /// # Errors
    /// Returns only a redacted remote error.
    fn get_start_page_token(&mut self) -> std::result::Result<String, RemoteError>;

    /// Fetches one direct-child metadata page for the durable folder frontier.
    ///
    /// # Errors
    /// Returns only a redacted remote error.
    fn scan_folder_page(
        &mut self,
        request: &ScanRequest,
    ) -> std::result::Result<ScanPage, RemoteError>;

    /// Fetches one Changes page from the exact supplied cursor.
    ///
    /// # Errors
    /// Returns only a redacted remote error.
    fn changes_page(&mut self, page_token: &str) -> std::result::Result<ChangesPage, RemoteError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialSyncProgress {
    StartTokenCaptured,
    ScanPageCommitted,
    ScanComplete,
    ChangesPageCommitted,
    Ready,
}

/// Advances exactly one durable initial-sync step.
///
/// Network results are committed only through `SyncStore` transactions. A
/// caller may safely repeat this function after a crash.
///
/// # Errors
/// Returns typed local validation/storage errors or a redacted remote error.
pub fn advance_initial_sync<C: DriveClient>(
    store: &mut SyncStore,
    client: &mut C,
    now_unix_ms: u64,
) -> Result<InitialSyncProgress> {
    let state = store.vault_state()?.ok_or(Error::InvalidStateTransition)?;
    if state.account_id.is_none() {
        return Err(Error::BindingRequiresAccount);
    }
    match state.phase {
        SyncPhase::NeedStartToken => {
            let token = client.get_start_page_token().map_err(Error::Remote)?;
            validate_remote_token(&token)?;
            store.begin_initial_scan(&token, now_unix_ms)?;
            Ok(InitialSyncProgress::StartTokenCaptured)
        }
        SyncPhase::Scanning => {
            let request = store.scan_request()?.ok_or(Error::InvalidStateTransition)?;
            let page = client.scan_folder_page(&request).map_err(Error::Remote)?;
            page.validate()?;
            store.apply_scan_page(request.page_token.as_deref(), &page, now_unix_ms)?;
            let scan_complete = store
                .vault_state()?
                .is_some_and(|current| current.phase == SyncPhase::Draining);
            Ok(if scan_complete {
                InitialSyncProgress::ScanComplete
            } else {
                InitialSyncProgress::ScanPageCommitted
            })
        }
        SyncPhase::Draining => {
            let cursor = state
                .changes_page_token
                .as_deref()
                .ok_or(Error::InvalidStateTransition)?;
            let page = match client.changes_page(cursor) {
                Ok(page) => page,
                Err(error) if matches!(error.code(), "cursor_expired" | "cursor_ambiguous") => {
                    store.mark_rescan_required(now_unix_ms)?;
                    return Err(Error::RescanRequired);
                }
                Err(error) => return Err(Error::Remote(error)),
            };
            page.validate()?;
            let complete = page.new_start_page_token.is_some();
            store.apply_changes_page(cursor, &page, now_unix_ms)?;
            Ok(if complete {
                InitialSyncProgress::Ready
            } else {
                InitialSyncProgress::ChangesPageCommitted
            })
        }
        SyncPhase::Ready => Ok(InitialSyncProgress::Ready),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncPhase {
    NeedStartToken,
    Scanning,
    Draining,
    Ready,
}

impl SyncPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NeedStartToken => "need_start_token",
            Self::Scanning => "scanning",
            Self::Draining => "draining",
            Self::Ready => "ready",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "need_start_token" => Ok(Self::NeedStartToken),
            "scanning" => Ok(Self::Scanning),
            "draining" => Ok(Self::Draining),
            "ready" => Ok(Self::Ready),
            _ => Err(Error::InvalidSchema),
        }
    }
}

pub(crate) fn validate_remote_id(value: &str) -> Result<()> {
    if (1..=512).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(Error::InvalidRemoteId)
    }
}

pub(crate) fn validate_remote_token(value: &str) -> Result<()> {
    if (1..=4096).contains(&value.len()) && !value.bytes().any(|byte| byte.is_ascii_control()) {
        Ok(())
    } else {
        Err(Error::InvalidRemoteToken)
    }
}

pub(crate) fn validate_content_path(value: &str) -> Result<()> {
    let path = VaultPath::from_portable(value).map_err(|_| Error::InvalidPortablePath)?;
    let collision_key = path.collision_key();
    let protected = matches!(
        collision_key.split('/').next(),
        Some(".obsidian" | ".trash")
    );
    if path.as_str() != value || path.is_obsidian_metadata() || protected {
        return Err(Error::InvalidPortablePath);
    }
    Ok(())
}

/// Returns whether a portable path is eligible for ordinary sync content.
/// Protected metadata/trash roots and non-canonical paths are rejected.
#[must_use]
pub fn is_valid_sync_content_path(value: &str) -> bool {
    validate_content_path(value).is_ok()
}

pub(crate) fn validate_revision(value: &str) -> Result<()> {
    if is_lower_hex_hash(value) {
        Ok(())
    } else {
        Err(Error::InvalidRevision)
    }
}

pub(crate) fn validate_redacted_code(value: &str) -> Result<()> {
    if (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(Error::InvalidErrorCode)
    }
}

pub(crate) fn validate_private_reference(value: &str) -> Result<()> {
    if (1..=256).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
        && !matches!(value, "." | "..")
    {
        Ok(())
    } else {
        Err(Error::InvalidTransferEvidence)
    }
}

pub(crate) fn u64_to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::InvalidTimestamp)
}

pub(crate) fn parse_uuid(value: &str) -> Result<Uuid> {
    let id = Uuid::parse_str(value).map_err(|_| Error::InvalidSchema)?;
    if id.is_nil() || id.to_string() != value {
        return Err(Error::InvalidSchema);
    }
    Ok(id)
}

fn validate_entries(entries: &[RemoteEntry]) -> Result<()> {
    for entry in entries {
        entry.validate()?;
    }
    Ok(())
}

fn is_lower_hex_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
