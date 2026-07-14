use crate::{
    parse_uuid, u64_to_i64, validate_content_path, validate_redacted_code, validate_remote_id,
    validate_remote_token, validate_revision, ChangesPage, Error, RemoteChange, RemoteEntry,
    RemoteEntryKind, Result, ScanPage, ScanRequest, SyncPhase, VerifiedRemoteBinding,
    MAX_SCAN_FRONTIER_FOLDERS,
};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use fs2::FileExt;
use myvault_private_fs as private_fs;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const ROOT_DIRECTORY: &str = "sync-state";
const VERSION_DIRECTORY: &str = "v1";
const VAULTS_DIRECTORY: &str = "vaults";
const LEASE_NAME: &str = "sync-operation.lock";
const DATABASE_NAME: &str = "myvault-sync.sqlite3";

const VAULT_STATE_SCHEMA_V1: &str = "CREATE TABLE vault_state (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    vault_id TEXT NOT NULL UNIQUE,
    remote_root_id TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('need_start_token', 'scanning', 'draining', 'ready')),
    start_token TEXT,
    scan_page_token TEXT,
    changes_page_token TEXT,
    durable_cursor TEXT,
    updated_at_unix_ms INTEGER NOT NULL CHECK (updated_at_unix_ms >= 0)
)";
const VAULT_STATE_SCHEMA: &str = "CREATE TABLE vault_state (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    vault_id TEXT NOT NULL UNIQUE,
    remote_root_id TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('need_start_token', 'scanning', 'draining', 'ready')),
    start_token TEXT,
    scan_page_token TEXT,
    changes_page_token TEXT,
    durable_cursor TEXT,
    updated_at_unix_ms INTEGER NOT NULL CHECK (updated_at_unix_ms >= 0),
    account_id TEXT,
    rescan_required INTEGER NOT NULL CHECK (rescan_required IN (0, 1))
)";
const REMOTE_ENTRIES_SCHEMA: &str = "CREATE TABLE remote_entries (
    file_id TEXT PRIMARY KEY NOT NULL,
    parent_id TEXT NOT NULL,
    portable_path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'folder')),
    content_hash_algorithm TEXT CHECK (content_hash_algorithm IN ('md5', 'sha1', 'sha256')),
    content_hash TEXT,
    remote_revision TEXT NOT NULL,
    base_local_revision TEXT,
    base_remote_revision TEXT,
    base_content_hash TEXT
)";
const REMOTE_ENTRIES_INDEX_SCHEMA: &str =
    "CREATE INDEX remote_entries_path_idx ON remote_entries(portable_path COLLATE BINARY)";
const REMOTE_ENTRIES_PREVIEW_INDEX_SCHEMA: &str = "CREATE INDEX remote_entries_preview_idx
    ON remote_entries(portable_path COLLATE BINARY, file_id COLLATE BINARY)";
const SCAN_FRONTIER_SCHEMA: &str = "CREATE TABLE scan_frontier (
    sequence INTEGER PRIMARY KEY NOT NULL,
    folder_id TEXT NOT NULL UNIQUE,
    portable_path TEXT NOT NULL,
    page_token TEXT
)";
const SYNC_JOBS_SCHEMA: &str = "CREATE TABLE sync_jobs (
    operation_id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('upload', 'download', 'move', 'trash')),
    path TEXT NOT NULL,
    destination_path TEXT,
    remote_file_id TEXT,
    expected_local_revision TEXT,
    state TEXT NOT NULL CHECK (state IN ('pending', 'running', 'retry_scheduled', 'needs_reconcile', 'completed')),
    attempt_count INTEGER NOT NULL CHECK (attempt_count >= 0),
    next_attempt_at_unix_ms INTEGER NOT NULL CHECK (next_attempt_at_unix_ms >= 0),
    created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
    last_error_code TEXT
)";
const SYNC_JOBS_INDEX_SCHEMA: &str = "CREATE INDEX sync_jobs_due_idx
    ON sync_jobs(state, next_attempt_at_unix_ms, created_at_unix_ms, operation_id)";
const SYNC_HISTORY_SCHEMA: &str = "CREATE TABLE sync_history (
    event_id INTEGER PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    outcome_code TEXT NOT NULL,
    occurred_at_unix_ms INTEGER NOT NULL CHECK (occurred_at_unix_ms >= 0)
)";
const CHANGE_BATCH_SCHEMA: &str = "CREATE TABLE change_batch (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    batch_id TEXT NOT NULL UNIQUE,
    expected_cursor TEXT NOT NULL,
    next_cursor TEXT NOT NULL
)";
const CHANGE_BATCH_MUTATIONS_SCHEMA: &str = "CREATE TABLE change_batch_mutations (
    batch_id TEXT NOT NULL,
    mutation_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'applying', 'committed')),
    PRIMARY KEY (batch_id, mutation_id),
    FOREIGN KEY (batch_id) REFERENCES change_batch(batch_id) ON DELETE CASCADE
)";

const SCHEMA_OBJECTS_V1: [(&str, &str, &str); 8] = [
    ("table", "vault_state", VAULT_STATE_SCHEMA_V1),
    ("table", "remote_entries", REMOTE_ENTRIES_SCHEMA),
    (
        "index",
        "remote_entries_path_idx",
        REMOTE_ENTRIES_INDEX_SCHEMA,
    ),
    ("table", "sync_jobs", SYNC_JOBS_SCHEMA),
    ("index", "sync_jobs_due_idx", SYNC_JOBS_INDEX_SCHEMA),
    ("table", "sync_history", SYNC_HISTORY_SCHEMA),
    ("table", "change_batch", CHANGE_BATCH_SCHEMA),
    (
        "table",
        "change_batch_mutations",
        CHANGE_BATCH_MUTATIONS_SCHEMA,
    ),
];

const SCHEMA_OBJECTS: [(&str, &str, &str); 10] = [
    ("table", "vault_state", VAULT_STATE_SCHEMA),
    ("table", "remote_entries", REMOTE_ENTRIES_SCHEMA),
    (
        "index",
        "remote_entries_path_idx",
        REMOTE_ENTRIES_INDEX_SCHEMA,
    ),
    (
        "index",
        "remote_entries_preview_idx",
        REMOTE_ENTRIES_PREVIEW_INDEX_SCHEMA,
    ),
    ("table", "scan_frontier", SCAN_FRONTIER_SCHEMA),
    ("table", "sync_jobs", SYNC_JOBS_SCHEMA),
    ("index", "sync_jobs_due_idx", SYNC_JOBS_INDEX_SCHEMA),
    ("table", "sync_history", SYNC_HISTORY_SCHEMA),
    ("table", "change_batch", CHANGE_BATCH_SCHEMA),
    (
        "table",
        "change_batch_mutations",
        CHANGE_BATCH_MUTATIONS_SCHEMA,
    ),
];

pub const SCHEMA_VERSION: i64 = 2;
pub const MAX_REMOTE_PREVIEW_PAGE_SIZE: usize = 200;

/// Exact residual risk inherited from bundled `SQLite`'s ambient-path VFS.
pub const SQLITE_OPEN_RESIDUAL_RISK: &str = "bundled SQLite opens ambient paths; a custom descriptor-relative VFS is required to resist a hostile same-user directory rename during sqlite3_open_v2";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindOutcome {
    Created,
    AlreadyBound,
    LegacyBindingConfirmed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultSyncState {
    pub vault_id: Uuid,
    pub account_id: Option<String>,
    pub remote_root_id: String,
    pub phase: SyncPhase,
    pub start_token: Option<String>,
    pub scan_page_token: Option<String>,
    pub changes_page_token: Option<String>,
    pub durable_cursor: Option<String>,
    pub rescan_required: bool,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemotePreviewCursor {
    pub path: String,
    pub file_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemotePreviewEntry {
    pub file_id: String,
    pub parent_id: String,
    pub path: String,
    pub kind: RemoteEntryKind,
    pub path_collision: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemotePreviewPage {
    pub entries: Vec<RemotePreviewEntry>,
    pub next_after: Option<RemotePreviewCursor>,
    pub has_more: bool,
    pub total_entries: u64,
    pub colliding_entries: u64,
    pub rescan_required: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueJobKind {
    Upload,
    Download,
    Move,
    Trash,
}

impl QueueJobKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
            Self::Move => "move",
            Self::Trash => "trash",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "upload" => Ok(Self::Upload),
            "download" => Ok(Self::Download),
            "move" => Ok(Self::Move),
            "trash" => Ok(Self::Trash),
            _ => Err(Error::InvalidSchema),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    Pending,
    Running,
    RetryScheduled,
    NeedsReconcile,
    Completed,
}

impl JobState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::RetryScheduled => "retry_scheduled",
            Self::NeedsReconcile => "needs_reconcile",
            Self::Completed => "completed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "retry_scheduled" => Ok(Self::RetryScheduled),
            "needs_reconcile" => Ok(Self::NeedsReconcile),
            "completed" => Ok(Self::Completed),
            _ => Err(Error::InvalidSchema),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueJob {
    operation_id: Uuid,
    kind: QueueJobKind,
    path: String,
    destination_path: Option<String>,
    remote_file_id: Option<String>,
    expected_local_revision: Option<String>,
    state: JobState,
    attempt_count: u32,
    next_attempt_at_unix_ms: u64,
    created_at_unix_ms: u64,
    last_error_code: Option<String>,
}

impl QueueJob {
    /// Creates one validated pending queue job without content or credentials.
    ///
    /// # Errors
    /// Rejects nil IDs, protected paths, malformed revisions, IDs, or timestamps.
    pub fn new(
        operation_id: Uuid,
        kind: QueueJobKind,
        path: impl Into<String>,
        destination_path: Option<String>,
        remote_file_id: Option<String>,
        expected_local_revision: Option<String>,
        created_at_unix_ms: u64,
    ) -> Result<Self> {
        let job = Self {
            operation_id,
            kind,
            path: path.into(),
            destination_path,
            remote_file_id,
            expected_local_revision,
            state: JobState::Pending,
            attempt_count: 0,
            next_attempt_at_unix_ms: created_at_unix_ms,
            created_at_unix_ms,
            last_error_code: None,
        };
        job.validate()?;
        Ok(job)
    }

    #[must_use]
    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    #[must_use]
    pub const fn kind(&self) -> QueueJobKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn destination_path(&self) -> Option<&str> {
        self.destination_path.as_deref()
    }

    #[must_use]
    pub fn remote_file_id(&self) -> Option<&str> {
        self.remote_file_id.as_deref()
    }

    #[must_use]
    pub fn expected_local_revision(&self) -> Option<&str> {
        self.expected_local_revision.as_deref()
    }

    #[must_use]
    pub const fn state(&self) -> JobState {
        self.state
    }

    #[must_use]
    pub const fn attempt_count(&self) -> u32 {
        self.attempt_count
    }

    #[must_use]
    pub const fn next_attempt_at_unix_ms(&self) -> u64 {
        self.next_attempt_at_unix_ms
    }

    #[must_use]
    pub const fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }

    #[must_use]
    pub fn last_error_code(&self) -> Option<&str> {
        self.last_error_code.as_deref()
    }

    fn validate(&self) -> Result<()> {
        if self.operation_id.is_nil() {
            return Err(Error::QueueCollision);
        }
        validate_content_path(&self.path)?;
        match (self.kind, self.destination_path.as_deref()) {
            (QueueJobKind::Move, Some(destination)) => validate_content_path(destination)?,
            (QueueJobKind::Move, None) | (_, Some(_)) => return Err(Error::InvalidPortablePath),
            (_, None) => {}
        }
        if let Some(file_id) = &self.remote_file_id {
            validate_remote_id(file_id)?;
        }
        if matches!(
            self.kind,
            QueueJobKind::Download | QueueJobKind::Move | QueueJobKind::Trash
        ) && self.remote_file_id.is_none()
        {
            return Err(Error::InvalidRemoteId);
        }
        if let Some(revision) = &self.expected_local_revision {
            validate_revision(revision)?;
        }
        if let Some(code) = &self.last_error_code {
            validate_redacted_code(code)?;
        }
        u64_to_i64(self.next_attempt_at_unix_ms)?;
        u64_to_i64(self.created_at_unix_ms)?;
        Ok(())
    }

    fn same_request(&self, other: &Self) -> bool {
        self.operation_id == other.operation_id
            && self.kind == other.kind
            && self.path == other.path
            && self.destination_path == other.destination_path
            && self.remote_file_id == other.remote_file_id
            && self.expected_local_revision == other.expected_local_revision
            && self.created_at_unix_ms == other.created_at_unix_ms
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    Enqueued,
    AlreadyPresent,
    AlreadyCompleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalMutationState {
    Pending,
    Applying,
    Committed,
}

impl LocalMutationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applying => "applying",
            Self::Committed => "committed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "applying" => Ok(Self::Applying),
            "committed" => Ok(Self::Committed),
            _ => Err(Error::InvalidSchema),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalMutationStatus {
    pub mutation_id: String,
    pub state: LocalMutationState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeBatch {
    pub batch_id: Uuid,
    pub expected_cursor: String,
    pub next_cursor: String,
    pub declared_mutations: u64,
    pub applying_mutations: u64,
    pub committed_mutations: u64,
}

pub struct SyncStore {
    connection: Connection,
    database_path: PathBuf,
    vault_id: Uuid,
    _lease_file: std::fs::File,
    _private_root: Dir,
    _vault_directory: Dir,
}

impl SyncStore {
    /// Opens one private, vault-specific operational sync database.
    ///
    /// The app-data root and Vault root must already exist and be disjoint.
    /// Existing malformed or newer schemas are preserved and rejected.
    ///
    /// # Errors
    /// Fails closed for unsafe storage, invalid IDs, corrupt evidence, or migration failures.
    pub fn open(app_data_root: &Path, vault_root: &Path, vault_id: Uuid) -> Result<Self> {
        if vault_id.is_nil() {
            return Err(Error::InvalidVaultId);
        }
        let canonical_app_root = app_data_root.canonicalize()?;
        let private_root = private_fs::open_private_disjoint_root(app_data_root, vault_root)?;
        let sync_root = private_fs::create_or_open_private_dir(&private_root, ROOT_DIRECTORY)?;
        let version = private_fs::create_or_open_private_dir(&sync_root, VERSION_DIRECTORY)?;
        let vaults = private_fs::create_or_open_private_dir(&version, VAULTS_DIRECTORY)?;
        let vault_directory =
            private_fs::create_or_open_private_dir(&vaults, vault_id.to_string())?;
        let lease_file = acquire_sync_lease(&vault_directory)?;
        let database_path = canonical_app_root
            .join(ROOT_DIRECTORY)
            .join(VERSION_DIRECTORY)
            .join(VAULTS_DIRECTORY)
            .join(vault_id.to_string())
            .join(DATABASE_NAME);

        let existed = vault_directory.symlink_metadata(DATABASE_NAME).is_ok();
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .follow(FollowSymlinks::No);
        let file = vault_directory.open_with(DATABASE_NAME, &options)?;
        if !existed {
            private_fs::set_private_file_permissions(&file)?;
        }
        private_fs::verify_private_file(&file, 1)?;
        file.sync_all()?;
        if !existed {
            private_fs::sync_directory(&vault_directory)?;
        }
        drop(file);

        let mut connection = Connection::open(&database_path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "temp_store", "MEMORY")?;
        migrate(&mut connection)?;
        connection.pragma_update(None, "journal_mode", "DELETE")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        private_fs::open_private_file(&vault_directory, DATABASE_NAME, 1)?;

        let mut store = Self {
            connection,
            database_path,
            vault_id,
            _lease_file: lease_file,
            _private_root: private_root,
            _vault_directory: vault_directory,
        };
        let _ = load_state(&store.connection, store.vault_id)?;
        store.recover_interrupted_jobs()?;
        Ok(store)
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Returns the applied schema version.
    ///
    /// # Errors
    /// Returns a database error if the pragma cannot be read.
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    /// Binds this local Vault to one exact remote root without silent rebinding.
    ///
    /// # Errors
    /// Rejects invalid IDs or a different existing remote binding.
    pub fn bind_remote_root(
        &mut self,
        binding: &VerifiedRemoteBinding,
        now_unix_ms: u64,
    ) -> Result<BindOutcome> {
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        if let Some(state) = load_state(&transaction, self.vault_id)? {
            if state.remote_root_id != binding.remote_root_id() {
                return Err(Error::BindingCollision);
            }
            if let Some(account_id) = state.account_id {
                if account_id != binding.account_id() {
                    return Err(Error::BindingCollision);
                }
                transaction.commit()?;
                return Ok(BindOutcome::AlreadyBound);
            }
            transaction.execute(
                "UPDATE vault_state
                 SET account_id = ?1, phase = ?2, start_token = NULL,
                     scan_page_token = NULL, changes_page_token = NULL,
                     durable_cursor = NULL, rescan_required = 1,
                     updated_at_unix_ms = ?3
                 WHERE singleton = 1 AND remote_root_id = ?4 AND account_id IS NULL",
                params![
                    binding.account_id(),
                    SyncPhase::NeedStartToken.as_str(),
                    now,
                    binding.remote_root_id()
                ],
            )?;
            transaction.execute("DELETE FROM scan_frontier", [])?;
            transaction.commit()?;
            return Ok(BindOutcome::LegacyBindingConfirmed);
        }
        transaction.execute(
            "INSERT INTO vault_state(
                singleton, vault_id, remote_root_id, phase, start_token,
                scan_page_token, changes_page_token, durable_cursor, updated_at_unix_ms,
                account_id, rescan_required
             ) VALUES (1, ?1, ?2, ?3, NULL, NULL, NULL, NULL, ?4, ?5, 0)",
            params![
                self.vault_id.to_string(),
                binding.remote_root_id(),
                SyncPhase::NeedStartToken.as_str(),
                now,
                binding.account_id()
            ],
        )?;
        transaction.commit()?;
        Ok(BindOutcome::Created)
    }

    /// Reads the current durable state for this vault.
    ///
    /// # Errors
    /// Rejects malformed persisted values.
    pub fn vault_state(&self) -> Result<Option<VaultSyncState>> {
        load_state(&self.connection, self.vault_id)
    }

    /// Verifies that the active durable binding matches the provider identity.
    ///
    /// # Errors
    /// Rejects an absent, unverified, or different account/root binding.
    pub fn verify_remote_binding(&self, binding: &VerifiedRemoteBinding) -> Result<()> {
        let state = self.vault_state()?.ok_or(Error::InvalidStateTransition)?;
        let account_id = state.account_id.ok_or(Error::BindingRequiresAccount)?;
        if account_id != binding.account_id() || state.remote_root_id != binding.remote_root_id() {
            return Err(Error::BindingCollision);
        }
        Ok(())
    }

    /// Persists the pre-scan Changes token and enters `Scanning`.
    ///
    /// # Errors
    /// Rejects invalid tokens or an unexpected phase.
    pub fn begin_initial_scan(&mut self, start_token: &str, now_unix_ms: u64) -> Result<()> {
        validate_remote_token(start_token)?;
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let state = require_state(&transaction, self.vault_id)?;
        if state.phase != SyncPhase::NeedStartToken {
            return Err(Error::InvalidStateTransition);
        }
        if state.account_id.is_none() {
            return Err(Error::BindingRequiresAccount);
        }
        transaction.execute("DELETE FROM remote_entries", [])?;
        transaction.execute("DELETE FROM scan_frontier", [])?;
        transaction.execute(
            "INSERT INTO scan_frontier(sequence, folder_id, portable_path, page_token)
             VALUES (1, ?1, '', NULL)",
            [state.remote_root_id.as_str()],
        )?;
        let changed = transaction.execute(
            "UPDATE vault_state
             SET phase = ?1, start_token = ?2, scan_page_token = NULL,
                 changes_page_token = NULL, durable_cursor = NULL, rescan_required = 0,
                 updated_at_unix_ms = ?3
             WHERE singleton = 1 AND vault_id = ?4 AND phase = ?5",
            params![
                SyncPhase::Scanning.as_str(),
                start_token,
                now,
                self.vault_id.to_string(),
                SyncPhase::NeedStartToken.as_str()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns the next durable direct-child folder request.
    ///
    /// # Errors
    /// Rejects missing, malformed, or cursor-inconsistent frontier evidence.
    pub fn scan_request(&self) -> Result<Option<ScanRequest>> {
        let state = self.vault_state()?.ok_or(Error::InvalidStateTransition)?;
        if state.phase != SyncPhase::Scanning {
            return Ok(None);
        }
        let request = self
            .connection
            .query_row(
                "SELECT folder_id, portable_path, page_token
                 FROM scan_frontier ORDER BY sequence LIMIT 1",
                [],
                |row| {
                    Ok(ScanRequest {
                        folder_id: row.get(0)?,
                        folder_path: row.get(1)?,
                        page_token: row.get(2)?,
                    })
                },
            )
            .optional()?
            .ok_or(Error::InvalidSchema)?;
        validate_remote_id(&request.folder_id)?;
        validate_frontier_path(&request.folder_path)?;
        if let Some(token) = &request.page_token {
            validate_remote_token(token)?;
        }
        if state.scan_page_token != request.page_token {
            return Err(Error::CursorMismatch);
        }
        Ok(Some(request))
    }

    /// Applies one recursive scan page and advances its page token atomically.
    ///
    /// # Errors
    /// Rejects malformed metadata, protected paths, or an unexpected phase.
    pub fn apply_scan_page(
        &mut self,
        expected_page_token: Option<&str>,
        page: &ScanPage,
        now_unix_ms: u64,
    ) -> Result<()> {
        if let Some(token) = expected_page_token {
            validate_remote_token(token)?;
        }
        page.validate()?;
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let state = require_state(&transaction, self.vault_id)?;
        if state.phase != SyncPhase::Scanning {
            return Err(Error::InvalidStateTransition);
        }
        if state.scan_page_token.as_deref() != expected_page_token {
            return Err(Error::CursorMismatch);
        }
        let current = load_frontier_head(&transaction)?.ok_or(Error::InvalidSchema)?;
        if current.page_token.as_deref() != expected_page_token {
            return Err(Error::CursorMismatch);
        }
        validate_scan_page_children(&transaction, &state.remote_root_id, &current, page)?;
        for entry in &page.entries {
            upsert_remote_entry(&transaction, entry)?;
        }
        enqueue_child_folders(&transaction, &current, &page.entries)?;
        if let Some(next) = &page.next_page_token {
            transaction.execute(
                "UPDATE scan_frontier SET page_token = ?1 WHERE sequence = ?2",
                params![next, current.sequence],
            )?;
            transaction.execute(
                "UPDATE vault_state SET scan_page_token = ?1, updated_at_unix_ms = ?2
                 WHERE singleton = 1",
                params![next, now],
            )?;
        } else {
            transaction.execute(
                "DELETE FROM scan_frontier WHERE sequence = ?1",
                [current.sequence],
            )?;
            let next_frontier = load_frontier_head(&transaction)?;
            if let Some(next_request) = next_frontier {
                transaction.execute(
                    "UPDATE vault_state SET scan_page_token = ?1, updated_at_unix_ms = ?2
                     WHERE singleton = 1",
                    params![next_request.page_token, now],
                )?;
            } else {
                let start = state.start_token.ok_or(Error::InvalidStateTransition)?;
                transaction.execute(
                    "UPDATE vault_state
                     SET phase = ?1, scan_page_token = NULL, changes_page_token = ?2,
                         updated_at_unix_ms = ?3
                     WHERE singleton = 1",
                    params![SyncPhase::Draining.as_str(), start, now],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Applies one Changes page and its next/durable token atomically.
    ///
    /// # Errors
    /// Rejects malformed changes, cursor mismatches, or an unexpected phase.
    pub fn apply_changes_page(
        &mut self,
        expected_cursor: &str,
        page: &ChangesPage,
        now_unix_ms: u64,
    ) -> Result<()> {
        validate_remote_token(expected_cursor)?;
        page.validate()?;
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let state = require_state(&transaction, self.vault_id)?;
        if state.phase != SyncPhase::Draining {
            return Err(Error::InvalidStateTransition);
        }
        if state.changes_page_token.as_deref() != Some(expected_cursor) {
            return Err(Error::CursorMismatch);
        }
        for change in &page.changes {
            match change {
                RemoteChange::Upsert(entry) => upsert_remote_entry(&transaction, entry)?,
                RemoteChange::Removed { file_id } => {
                    transaction
                        .execute("DELETE FROM remote_entries WHERE file_id = ?1", [file_id])?;
                }
            }
        }
        if let Some(next) = &page.next_page_token {
            transaction.execute(
                "UPDATE vault_state SET changes_page_token = ?1, updated_at_unix_ms = ?2
                 WHERE singleton = 1",
                params![next, now],
            )?;
        } else {
            let durable = page
                .new_start_page_token
                .as_deref()
                .ok_or(Error::InvalidRemoteEntry)?;
            transaction.execute(
                "UPDATE vault_state
                 SET phase = ?1, start_token = NULL, scan_page_token = NULL,
                     changes_page_token = NULL, durable_cursor = ?2, updated_at_unix_ms = ?3
                 WHERE singleton = 1",
                params![SyncPhase::Ready.as_str(), durable, now],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns the number of persisted remote entries.
    ///
    /// # Errors
    /// Returns a database error or invalid count error.
    pub fn remote_entry_count(&self) -> Result<u64> {
        query_count(&self.connection, "SELECT COUNT(*) FROM remote_entries")
    }

    /// Returns one bounded, deterministic remote metadata preview page.
    ///
    /// # Errors
    /// Rejects an invalid cursor/page size or malformed persisted evidence.
    pub fn remote_preview(
        &self,
        after: Option<&RemotePreviewCursor>,
        limit: usize,
    ) -> Result<RemotePreviewPage> {
        if !(1..=MAX_REMOTE_PREVIEW_PAGE_SIZE).contains(&limit) {
            return Err(Error::InvalidPreviewLimit);
        }
        if let Some(cursor) = after {
            validate_content_path(&cursor.path).map_err(|_| Error::InvalidPreviewCursor)?;
            validate_remote_id(&cursor.file_id).map_err(|_| Error::InvalidPreviewCursor)?;
        }
        let state = self.vault_state()?.ok_or(Error::InvalidStateTransition)?;
        let fetch_limit = i64::try_from(limit + 1).map_err(|_| Error::InvalidPreviewLimit)?;
        let mut statement = self.connection.prepare(
            "SELECT entry.file_id, entry.parent_id, entry.portable_path, entry.kind,
                    EXISTS(
                        SELECT 1 FROM remote_entries duplicate
                        WHERE duplicate.portable_path = entry.portable_path
                          AND duplicate.file_id <> entry.file_id
                    )
             FROM remote_entries entry
             WHERE (?1 IS NULL OR entry.portable_path > ?1
                    OR (entry.portable_path = ?1 AND entry.file_id > ?2))
             ORDER BY entry.portable_path COLLATE BINARY, entry.file_id COLLATE BINARY
             LIMIT ?3",
        )?;
        let (after_path, after_id) = after.map_or((None, None), |cursor| {
            (Some(cursor.path.as_str()), Some(cursor.file_id.as_str()))
        });
        let mut entries = statement
            .query_map(params![after_path, after_id, fetch_limit], |row| {
                let kind: String = row.get(3)?;
                Ok(RemotePreviewEntry {
                    file_id: row.get(0)?,
                    parent_id: row.get(1)?,
                    path: row.get(2)?,
                    kind: if kind == "file" {
                        RemoteEntryKind::File
                    } else {
                        RemoteEntryKind::Folder
                    },
                    path_collision: row.get::<_, i64>(4)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let has_more = entries.len() > limit;
        if has_more {
            entries.pop();
        }
        let next_after = if has_more {
            let last = entries.last().ok_or(Error::InvalidSchema)?;
            Some(RemotePreviewCursor {
                path: last.path.clone(),
                file_id: last.file_id.clone(),
            })
        } else {
            None
        };
        let total_entries = self.remote_entry_count()?;
        let colliding_entries = query_count(
            &self.connection,
            "SELECT COUNT(*) FROM remote_entries entry
             WHERE EXISTS(
                SELECT 1 FROM remote_entries duplicate
                WHERE duplicate.portable_path = entry.portable_path
                  AND duplicate.file_id <> entry.file_id
             )",
        )?;
        Ok(RemotePreviewPage {
            entries,
            next_after,
            has_more,
            total_entries,
            colliding_entries,
            rescan_required: state.rescan_required,
        })
    }

    /// Invalidates scan cursors while preserving remote metadata as stale preview evidence.
    ///
    /// # Errors
    /// Rejects missing binding state, invalid timestamps, or storage failures.
    pub fn mark_rescan_required(&mut self, now_unix_ms: u64) -> Result<()> {
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        require_state(&transaction, self.vault_id)?;
        transaction.execute("DELETE FROM scan_frontier", [])?;
        transaction.execute(
            "UPDATE vault_state
             SET phase = ?1, start_token = NULL, scan_page_token = NULL,
                 changes_page_token = NULL, durable_cursor = NULL,
                 rescan_required = 1, updated_at_unix_ms = ?2
             WHERE singleton = 1",
            params![SyncPhase::NeedStartToken.as_str(), now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Enqueues a validated operation with exact-idempotent retry semantics.
    ///
    /// # Errors
    /// Rejects unbound state, invalid jobs, or mismatched operation-ID reuse.
    pub fn enqueue_job(&mut self, job: &QueueJob) -> Result<EnqueueOutcome> {
        job.validate()?;
        let transaction = self.connection.transaction()?;
        require_state(&transaction, self.vault_id)?;
        if let Some(existing) = load_job(&transaction, job.operation_id)? {
            if existing.same_request(job) {
                let outcome = if existing.state == JobState::Completed {
                    EnqueueOutcome::AlreadyCompleted
                } else {
                    EnqueueOutcome::AlreadyPresent
                };
                transaction.commit()?;
                return Ok(outcome);
            }
            return Err(Error::QueueCollision);
        }
        insert_job(&transaction, job)?;
        transaction.commit()?;
        Ok(EnqueueOutcome::Enqueued)
    }

    /// Claims the oldest due pending/retry job atomically.
    ///
    /// Jobs requiring reconciliation are never returned by this method.
    ///
    /// # Errors
    /// Returns database or persisted-schema errors.
    pub fn claim_next_job(&mut self, now_unix_ms: u64) -> Result<Option<QueueJob>> {
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let candidate = {
            let mut statement = transaction.prepare(
                "SELECT operation_id, kind, path, destination_path, remote_file_id,
                        expected_local_revision, state, attempt_count, next_attempt_at_unix_ms,
                        created_at_unix_ms, last_error_code
                 FROM sync_jobs
                 WHERE state IN ('pending', 'retry_scheduled')
                   AND next_attempt_at_unix_ms <= ?1
                 ORDER BY created_at_unix_ms, operation_id
                 LIMIT 1",
            )?;
            statement.query_row([now], row_to_job).optional()?
        };
        let Some(mut job) = candidate.transpose()? else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE sync_jobs SET state = ?1
             WHERE operation_id = ?2 AND state IN ('pending', 'retry_scheduled')",
            params![JobState::Running.as_str(), job.operation_id.to_string()],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        job.state = JobState::Running;
        transaction.commit()?;
        Ok(Some(job))
    }

    /// Schedules a verified-safe retry with a redacted error code.
    ///
    /// # Errors
    /// Only running or reconciled jobs can be scheduled.
    pub fn schedule_retry(
        &mut self,
        operation_id: Uuid,
        next_attempt_at_unix_ms: u64,
        error_code: &str,
    ) -> Result<()> {
        if operation_id.is_nil() {
            return Err(Error::JobNotFound);
        }
        validate_redacted_code(error_code)?;
        let next = u64_to_i64(next_attempt_at_unix_ms)?;
        let changed = self.connection.execute(
            "UPDATE sync_jobs
             SET state = ?1, attempt_count = attempt_count + 1,
                 next_attempt_at_unix_ms = ?2, last_error_code = ?3
             WHERE operation_id = ?4 AND state IN ('running', 'needs_reconcile')",
            params![
                JobState::RetryScheduled.as_str(),
                next,
                error_code,
                operation_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        Ok(())
    }

    /// Completes a verified job and records redacted history in one transaction.
    ///
    /// # Errors
    /// Rejects non-running jobs and invalid outcome codes.
    pub fn complete_verified_job(
        &mut self,
        operation_id: Uuid,
        outcome_code: &str,
        occurred_at_unix_ms: u64,
    ) -> Result<()> {
        if operation_id.is_nil() {
            return Err(Error::JobNotFound);
        }
        validate_redacted_code(outcome_code)?;
        let occurred = u64_to_i64(occurred_at_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let job = load_job(&transaction, operation_id)?.ok_or(Error::JobNotFound)?;
        if !matches!(job.state, JobState::Running | JobState::NeedsReconcile) {
            return Err(Error::InvalidStateTransition);
        }
        let changed = transaction.execute(
            "INSERT INTO sync_history(operation_id, outcome_code, occurred_at_unix_ms)
             VALUES (?1, ?2, ?3)",
            params![operation_id.to_string(), outcome_code, occurred],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        let changed = transaction.execute(
            "UPDATE sync_jobs
             SET state = ?1, next_attempt_at_unix_ms = ?2, last_error_code = NULL
             WHERE operation_id = ?3 AND state IN ('running', 'needs_reconcile')",
            params![
                JobState::Completed.as_str(),
                occurred,
                operation_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Reads one queue job by ID.
    ///
    /// # Errors
    /// Returns persisted-schema or database errors.
    pub fn job(&self, operation_id: Uuid) -> Result<Option<QueueJob>> {
        load_job(&self.connection, operation_id)
    }

    /// Returns the number of active queue jobs.
    ///
    /// # Errors
    /// Returns database or invalid count errors.
    pub fn queue_count(&self) -> Result<u64> {
        query_count(
            &self.connection,
            "SELECT COUNT(*) FROM sync_jobs WHERE state != 'completed'",
        )
    }

    /// Returns the number of redacted history entries.
    ///
    /// # Errors
    /// Returns database or invalid count errors.
    pub fn history_count(&self) -> Result<u64> {
        query_count(&self.connection, "SELECT COUNT(*) FROM sync_history")
    }

    /// Starts one durable incremental cursor batch.
    ///
    /// # Errors
    /// Rejects cursor mismatch, duplicate mutations, invalid IDs, or an active batch.
    pub fn begin_change_batch<I, S>(
        &mut self,
        batch_id: Uuid,
        expected_cursor: &str,
        next_cursor: &str,
        mutation_ids: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if batch_id.is_nil() {
            return Err(Error::InvalidStateTransition);
        }
        validate_remote_token(expected_cursor)?;
        validate_remote_token(next_cursor)?;
        let supplied = mutation_ids.into_iter().map(Into::into).collect::<Vec<_>>();
        let declared = supplied.iter().cloned().collect::<BTreeSet<_>>();
        if declared.len() != supplied.len() {
            return Err(Error::UnknownMutation);
        }
        for mutation in &declared {
            validate_redacted_code(mutation)?;
        }
        let transaction = self.connection.transaction()?;
        let state = require_state(&transaction, self.vault_id)?;
        if state.phase != SyncPhase::Ready
            || state.durable_cursor.as_deref() != Some(expected_cursor)
        {
            return Err(Error::CursorMismatch);
        }
        if transaction
            .query_row("SELECT 1 FROM change_batch LIMIT 1", [], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Err(Error::BatchAlreadyActive);
        }
        transaction.execute(
            "INSERT INTO change_batch(singleton, batch_id, expected_cursor, next_cursor)
             VALUES (1, ?1, ?2, ?3)",
            params![batch_id.to_string(), expected_cursor, next_cursor],
        )?;
        for mutation in declared {
            transaction.execute(
                "INSERT INTO change_batch_mutations(batch_id, mutation_id, state)
                 VALUES (?1, ?2, ?3)",
                params![
                    batch_id.to_string(),
                    mutation,
                    LocalMutationState::Pending.as_str()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Marks one declared local mutation as applying before touching the Vault.
    ///
    /// A process interruption after this durable transition leaves an explicit
    /// unknown outcome that must be reconciled before retrying.
    ///
    /// # Errors
    /// Rejects missing batches, wrong IDs, undeclared mutations, or non-pending state.
    pub fn begin_local_mutation(&mut self, batch_id: Uuid, mutation_id: &str) -> Result<()> {
        validate_redacted_code(mutation_id)?;
        let active = self.active_change_batch()?.ok_or(Error::NoActiveBatch)?;
        if active.batch_id != batch_id {
            return Err(Error::NoActiveBatch);
        }
        let changed = self.connection.execute(
            "UPDATE change_batch_mutations SET state = ?1
             WHERE batch_id = ?2 AND mutation_id = ?3 AND state = ?4",
            params![
                LocalMutationState::Applying.as_str(),
                batch_id.to_string(),
                mutation_id,
                LocalMutationState::Pending.as_str()
            ],
        )?;
        if changed == 1 {
            return Ok(());
        }
        match load_local_mutation_state(&self.connection, batch_id, mutation_id)? {
            Some(LocalMutationState::Applying) => Err(Error::MutationNeedsReconcile),
            Some(LocalMutationState::Committed) => Err(Error::InvalidStateTransition),
            Some(LocalMutationState::Pending) | None => Err(Error::UnknownMutation),
        }
    }

    /// Marks one applying local mutation committed after its guarded operation succeeds.
    ///
    /// # Errors
    /// Rejects missing batches, wrong batch IDs, or undeclared mutation IDs.
    pub fn mark_local_mutation_committed(
        &mut self,
        batch_id: Uuid,
        mutation_id: &str,
    ) -> Result<()> {
        validate_redacted_code(mutation_id)?;
        let active = self.active_change_batch()?.ok_or(Error::NoActiveBatch)?;
        if active.batch_id != batch_id {
            return Err(Error::NoActiveBatch);
        }
        let changed = self.connection.execute(
            "UPDATE change_batch_mutations SET state = ?1
             WHERE batch_id = ?2 AND mutation_id = ?3 AND state = ?4",
            params![
                LocalMutationState::Committed.as_str(),
                batch_id.to_string(),
                mutation_id,
                LocalMutationState::Applying.as_str()
            ],
        )?;
        if changed != 1 {
            return match load_local_mutation_state(&self.connection, batch_id, mutation_id)? {
                Some(LocalMutationState::Applying) => Err(Error::InvalidStateTransition),
                Some(LocalMutationState::Pending | LocalMutationState::Committed) => {
                    Err(Error::InvalidStateTransition)
                }
                None => Err(Error::UnknownMutation),
            };
        }
        Ok(())
    }

    /// Returns an applying mutation to pending only after remote/local absence is verified.
    ///
    /// # Errors
    /// Rejects missing batches, wrong IDs, undeclared mutations, or non-applying state.
    pub fn reset_local_mutation_after_verified_absence(
        &mut self,
        batch_id: Uuid,
        mutation_id: &str,
    ) -> Result<()> {
        validate_redacted_code(mutation_id)?;
        let active = self.active_change_batch()?.ok_or(Error::NoActiveBatch)?;
        if active.batch_id != batch_id {
            return Err(Error::NoActiveBatch);
        }
        let changed = self.connection.execute(
            "UPDATE change_batch_mutations SET state = ?1
             WHERE batch_id = ?2 AND mutation_id = ?3 AND state = ?4",
            params![
                LocalMutationState::Pending.as_str(),
                batch_id.to_string(),
                mutation_id,
                LocalMutationState::Applying.as_str()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        Ok(())
    }

    /// Reads all declared local mutations and their durable states.
    ///
    /// # Errors
    /// Rejects missing/wrong batches or malformed persisted mutation state.
    pub fn local_mutations(&self, batch_id: Uuid) -> Result<Vec<LocalMutationStatus>> {
        let active = self.active_change_batch()?.ok_or(Error::NoActiveBatch)?;
        if active.batch_id != batch_id {
            return Err(Error::NoActiveBatch);
        }
        load_local_mutations(&self.connection, batch_id)
    }

    /// Commits the next cursor only after all declared local mutations committed.
    ///
    /// # Errors
    /// Rejects missing/partial batches or a changed durable cursor.
    pub fn commit_change_batch(&mut self, batch_id: Uuid, now_unix_ms: u64) -> Result<()> {
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let batch = load_change_batch(&transaction)?.ok_or(Error::NoActiveBatch)?;
        if batch.batch_id != batch_id {
            return Err(Error::NoActiveBatch);
        }
        if batch.applying_mutations != 0 || batch.declared_mutations != batch.committed_mutations {
            return Err(Error::LocalMutationIncomplete);
        }
        let changed = transaction.execute(
            "UPDATE vault_state SET durable_cursor = ?1, updated_at_unix_ms = ?2
             WHERE singleton = 1 AND phase = ?3 AND durable_cursor = ?4",
            params![
                batch.next_cursor,
                now,
                SyncPhase::Ready.as_str(),
                batch.expected_cursor
            ],
        )?;
        if changed != 1 {
            return Err(Error::CursorMismatch);
        }
        transaction.execute("DELETE FROM change_batch WHERE singleton = 1", [])?;
        transaction.commit()?;
        Ok(())
    }

    /// Aborts the active batch while keeping the previous durable cursor.
    ///
    /// # Errors
    /// Rejects a missing or different active batch.
    pub fn abort_change_batch(&mut self, batch_id: Uuid) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let active = load_change_batch(&transaction)?.ok_or(Error::NoActiveBatch)?;
        if active.batch_id != batch_id {
            return Err(Error::NoActiveBatch);
        }
        if active.applying_mutations != 0 || active.committed_mutations != 0 {
            return Err(Error::MutationNeedsReconcile);
        }
        let changed = transaction.execute(
            "DELETE FROM change_batch
             WHERE singleton = 1 AND batch_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM change_batch_mutations
                   WHERE batch_id = ?1 AND state != 'pending'
               )",
            [batch_id.to_string()],
        )?;
        if changed != 1 {
            return Err(Error::MutationNeedsReconcile);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Reads the active incremental cursor batch.
    ///
    /// # Errors
    /// Rejects malformed persisted identifiers or counts.
    pub fn active_change_batch(&self) -> Result<Option<ChangeBatch>> {
        load_change_batch(&self.connection)
    }

    fn recover_interrupted_jobs(&mut self) -> Result<()> {
        self.connection.execute(
            "UPDATE sync_jobs SET state = ?1, last_error_code = 'interrupted_unknown_outcome'
             WHERE state = ?2",
            params![
                JobState::NeedsReconcile.as_str(),
                JobState::Running.as_str()
            ],
        )?;
        Ok(())
    }
}

fn acquire_sync_lease(vault_directory: &Dir) -> Result<std::fs::File> {
    let mut create = OpenOptions::new();
    create
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let (lease, created) = match vault_directory.open_with(LEASE_NAME, &create) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing = OpenOptions::new();
            existing.read(true).write(true).follow(FollowSymlinks::No);
            (vault_directory.open_with(LEASE_NAME, &existing)?, false)
        }
        Err(error) => return Err(error.into()),
    };
    if created {
        private_fs::set_private_file_permissions(&lease)?;
    }
    private_fs::verify_private_file(&lease, 1)?;
    if created {
        lease.sync_all()?;
        private_fs::sync_directory(vault_directory)?;
    }
    let lease = lease.into_std();
    if let Err(error) = FileExt::try_lock_exclusive(&lease) {
        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            return Err(Error::SyncLeaseHeld);
        }
        return Err(error.into());
    }
    Ok(lease)
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    let integrity: String = transaction.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::InvalidSchema);
    }
    let current: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(Error::UnsupportedSchema(current));
    }
    if current < 0 {
        return Err(Error::InvalidSchema);
    }
    if current == 0 {
        let existing: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type IN ('table', 'index', 'view', 'trigger')
               AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if existing != 0 {
            return Err(Error::InvalidSchema);
        }
        create_schema(&transaction)?;
    } else if current == 1 {
        if !schema_v1_is_valid(&transaction)? {
            return Err(Error::InvalidSchema);
        }
        migrate_v1_to_v2(&transaction)?;
    }
    if !schema_v2_is_valid(&transaction)? {
        return Err(Error::InvalidSchema);
    }
    transaction.commit()?;
    Ok(())
}

fn create_schema(transaction: &Transaction<'_>) -> Result<()> {
    for (_, _, statement) in SCHEMA_OBJECTS {
        transaction.execute_batch(statement)?;
    }
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn migrate_v1_to_v2(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "ALTER TABLE vault_state RENAME TO vault_state_v1;
         CREATE TABLE vault_state (
            singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
            vault_id TEXT NOT NULL UNIQUE,
            remote_root_id TEXT NOT NULL,
            phase TEXT NOT NULL CHECK (phase IN ('need_start_token', 'scanning', 'draining', 'ready')),
            start_token TEXT,
            scan_page_token TEXT,
            changes_page_token TEXT,
            durable_cursor TEXT,
            updated_at_unix_ms INTEGER NOT NULL CHECK (updated_at_unix_ms >= 0),
            account_id TEXT,
            rescan_required INTEGER NOT NULL CHECK (rescan_required IN (0, 1))
         );
         INSERT INTO vault_state(
            singleton, vault_id, remote_root_id, phase, start_token,
            scan_page_token, changes_page_token, durable_cursor, updated_at_unix_ms,
            account_id, rescan_required
         )
         SELECT singleton, vault_id, remote_root_id, 'need_start_token', NULL,
                NULL, NULL, NULL, updated_at_unix_ms, NULL, 1
         FROM vault_state_v1;
         DROP TABLE vault_state_v1;",
    )?;
    transaction.execute_batch(REMOTE_ENTRIES_PREVIEW_INDEX_SCHEMA)?;
    transaction.execute_batch(SCAN_FRONTIER_SCHEMA)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn schema_v1_is_valid(connection: &Connection) -> Result<bool> {
    if !schema_definitions_are_exact(connection, &SCHEMA_OBJECTS_V1)? {
        return Ok(false);
    }
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected = [
        "change_batch",
        "change_batch_mutations",
        "remote_entries",
        "sync_history",
        "sync_jobs",
        "vault_state",
    ];
    if tables.iter().map(String::as_str).ne(expected) {
        return Ok(false);
    }
    if !primary_schema_columns_are_valid(connection)?
        || !auxiliary_schema_columns_are_valid(connection)?
        || !index_has_columns(connection, "remote_entries_path_idx", &["portable_path"])?
        || !index_has_columns(
            connection,
            "sync_jobs_due_idx",
            &[
                "state",
                "next_attempt_at_unix_ms",
                "created_at_unix_ms",
                "operation_id",
            ],
        )?
    {
        return Ok(false);
    }
    let foreign_key_errors: i64 =
        connection.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    Ok(foreign_key_errors == 0)
}

fn schema_v2_is_valid(connection: &Connection) -> Result<bool> {
    if !schema_definitions_are_exact(connection, &SCHEMA_OBJECTS)? {
        return Ok(false);
    }
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected = [
        "change_batch",
        "change_batch_mutations",
        "remote_entries",
        "scan_frontier",
        "sync_history",
        "sync_jobs",
        "vault_state",
    ];
    if tables.iter().map(String::as_str).ne(expected) {
        return Ok(false);
    }
    if !primary_schema_columns_are_valid_v2(connection)?
        || !auxiliary_schema_columns_are_valid(connection)?
        || !index_has_columns(connection, "remote_entries_path_idx", &["portable_path"])?
        || !index_has_columns(
            connection,
            "remote_entries_preview_idx",
            &["portable_path", "file_id"],
        )?
        || !index_has_columns(
            connection,
            "sync_jobs_due_idx",
            &[
                "state",
                "next_attempt_at_unix_ms",
                "created_at_unix_ms",
                "operation_id",
            ],
        )?
    {
        return Ok(false);
    }
    let foreign_key_errors: i64 =
        connection.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    Ok(foreign_key_errors == 0)
}

fn primary_schema_columns_are_valid(connection: &Connection) -> Result<bool> {
    Ok(table_has_columns(
        connection,
        "vault_state",
        &[
            ("singleton", "INTEGER", true, 1),
            ("vault_id", "TEXT", true, 0),
            ("remote_root_id", "TEXT", true, 0),
            ("phase", "TEXT", true, 0),
            ("start_token", "TEXT", false, 0),
            ("scan_page_token", "TEXT", false, 0),
            ("changes_page_token", "TEXT", false, 0),
            ("durable_cursor", "TEXT", false, 0),
            ("updated_at_unix_ms", "INTEGER", true, 0),
        ],
    )? && table_has_columns(
        connection,
        "remote_entries",
        &[
            ("file_id", "TEXT", true, 1),
            ("parent_id", "TEXT", true, 0),
            ("portable_path", "TEXT", true, 0),
            ("kind", "TEXT", true, 0),
            ("content_hash_algorithm", "TEXT", false, 0),
            ("content_hash", "TEXT", false, 0),
            ("remote_revision", "TEXT", true, 0),
            ("base_local_revision", "TEXT", false, 0),
            ("base_remote_revision", "TEXT", false, 0),
            ("base_content_hash", "TEXT", false, 0),
        ],
    )? && table_has_columns(
        connection,
        "sync_jobs",
        &[
            ("operation_id", "TEXT", true, 1),
            ("kind", "TEXT", true, 0),
            ("path", "TEXT", true, 0),
            ("destination_path", "TEXT", false, 0),
            ("remote_file_id", "TEXT", false, 0),
            ("expected_local_revision", "TEXT", false, 0),
            ("state", "TEXT", true, 0),
            ("attempt_count", "INTEGER", true, 0),
            ("next_attempt_at_unix_ms", "INTEGER", true, 0),
            ("created_at_unix_ms", "INTEGER", true, 0),
            ("last_error_code", "TEXT", false, 0),
        ],
    )?)
}

fn primary_schema_columns_are_valid_v2(connection: &Connection) -> Result<bool> {
    Ok(table_has_columns(
        connection,
        "vault_state",
        &[
            ("singleton", "INTEGER", true, 1),
            ("vault_id", "TEXT", true, 0),
            ("remote_root_id", "TEXT", true, 0),
            ("phase", "TEXT", true, 0),
            ("start_token", "TEXT", false, 0),
            ("scan_page_token", "TEXT", false, 0),
            ("changes_page_token", "TEXT", false, 0),
            ("durable_cursor", "TEXT", false, 0),
            ("updated_at_unix_ms", "INTEGER", true, 0),
            ("account_id", "TEXT", false, 0),
            ("rescan_required", "INTEGER", true, 0),
        ],
    )? && table_has_columns(
        connection,
        "remote_entries",
        &[
            ("file_id", "TEXT", true, 1),
            ("parent_id", "TEXT", true, 0),
            ("portable_path", "TEXT", true, 0),
            ("kind", "TEXT", true, 0),
            ("content_hash_algorithm", "TEXT", false, 0),
            ("content_hash", "TEXT", false, 0),
            ("remote_revision", "TEXT", true, 0),
            ("base_local_revision", "TEXT", false, 0),
            ("base_remote_revision", "TEXT", false, 0),
            ("base_content_hash", "TEXT", false, 0),
        ],
    )? && table_has_columns(
        connection,
        "scan_frontier",
        &[
            ("sequence", "INTEGER", true, 1),
            ("folder_id", "TEXT", true, 0),
            ("portable_path", "TEXT", true, 0),
            ("page_token", "TEXT", false, 0),
        ],
    )? && table_has_columns(
        connection,
        "sync_jobs",
        &[
            ("operation_id", "TEXT", true, 1),
            ("kind", "TEXT", true, 0),
            ("path", "TEXT", true, 0),
            ("destination_path", "TEXT", false, 0),
            ("remote_file_id", "TEXT", false, 0),
            ("expected_local_revision", "TEXT", false, 0),
            ("state", "TEXT", true, 0),
            ("attempt_count", "INTEGER", true, 0),
            ("next_attempt_at_unix_ms", "INTEGER", true, 0),
            ("created_at_unix_ms", "INTEGER", true, 0),
            ("last_error_code", "TEXT", false, 0),
        ],
    )?)
}

fn auxiliary_schema_columns_are_valid(connection: &Connection) -> Result<bool> {
    Ok(table_has_columns(
        connection,
        "sync_history",
        &[
            ("event_id", "INTEGER", true, 1),
            ("operation_id", "TEXT", true, 0),
            ("outcome_code", "TEXT", true, 0),
            ("occurred_at_unix_ms", "INTEGER", true, 0),
        ],
    )? && table_has_columns(
        connection,
        "change_batch",
        &[
            ("singleton", "INTEGER", true, 1),
            ("batch_id", "TEXT", true, 0),
            ("expected_cursor", "TEXT", true, 0),
            ("next_cursor", "TEXT", true, 0),
        ],
    )? && table_has_columns(
        connection,
        "change_batch_mutations",
        &[
            ("batch_id", "TEXT", true, 1),
            ("mutation_id", "TEXT", true, 2),
            ("state", "TEXT", true, 0),
        ],
    )?)
}

fn schema_definitions_are_exact(
    connection: &Connection,
    expected_schema: &[(&str, &str, &str)],
) -> Result<bool> {
    let actual_objects = {
        let mut statement = connection.prepare(
            "SELECT type, name FROM sqlite_master
             WHERE type IN ('table', 'index', 'view', 'trigger')
               AND name NOT LIKE 'sqlite_%'",
        )?;
        let objects = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?;
        objects
    };
    let expected_objects = expected_schema
        .iter()
        .map(|(kind, name, _)| ((*kind).to_owned(), (*name).to_owned()))
        .collect::<BTreeSet<_>>();
    if actual_objects != expected_objects {
        return Ok(false);
    }

    for (kind, name, expected_sql) in expected_schema {
        let actual_sql = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
                params![kind, name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(actual_sql) = actual_sql else {
            return Ok(false);
        };
        if normalize_schema_sql(&actual_sql) != normalize_schema_sql(expected_sql) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn normalize_schema_sql(value: &str) -> String {
    value
        .trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn table_has_columns(
    connection: &Connection,
    table: &str,
    expected: &[(&str, &str, bool, i64)],
) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let columns = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(columns.len() == expected.len()
        && columns.iter().zip(expected).all(
            |((name, data_type, not_null, primary_key), expected)| {
                (name.as_str(), data_type.as_str(), *not_null, *primary_key) == *expected
            },
        ))
}

fn index_has_columns(connection: &Connection, index: &str, expected: &[&str]) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA index_info(\"{index}\")"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(2))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(columns
        .iter()
        .map(String::as_str)
        .eq(expected.iter().copied()))
}

fn load_state(connection: &Connection, expected_vault_id: Uuid) -> Result<Option<VaultSyncState>> {
    let row = connection
        .query_row(
            "SELECT vault_id, remote_root_id, phase, start_token, scan_page_token,
                    changes_page_token, durable_cursor, updated_at_unix_ms,
                    account_id, rescan_required
             FROM vault_state WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((
        vault_id,
        remote_root_id,
        phase,
        start_token,
        scan_page_token,
        changes_page_token,
        durable_cursor,
        updated_at,
        account_id,
        rescan_required,
    )) = row
    else {
        return Ok(None);
    };
    let vault_id = parse_uuid(&vault_id)?;
    if vault_id != expected_vault_id {
        return Err(Error::BindingCollision);
    }
    validate_remote_id(&remote_root_id)?;
    if let Some(account_id) = &account_id {
        validate_remote_id(account_id)?;
    }
    for token in [
        start_token.as_deref(),
        scan_page_token.as_deref(),
        changes_page_token.as_deref(),
        durable_cursor.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_remote_token(token)?;
    }
    let updated_at_unix_ms = u64::try_from(updated_at).map_err(|_| Error::InvalidSchema)?;
    let rescan_required = match rescan_required {
        0 => false,
        1 => true,
        _ => return Err(Error::InvalidSchema),
    };
    Ok(Some(VaultSyncState {
        vault_id,
        account_id,
        remote_root_id,
        phase: SyncPhase::parse(&phase)?,
        start_token,
        scan_page_token,
        changes_page_token,
        durable_cursor,
        rescan_required,
        updated_at_unix_ms,
    }))
}

#[derive(Clone)]
struct FrontierRow {
    sequence: i64,
    folder_id: String,
    portable_path: String,
    page_token: Option<String>,
}

fn load_frontier_head(connection: &Connection) -> Result<Option<FrontierRow>> {
    Ok(connection
        .query_row(
            "SELECT sequence, folder_id, portable_path, page_token
             FROM scan_frontier ORDER BY sequence LIMIT 1",
            [],
            |row| {
                Ok(FrontierRow {
                    sequence: row.get(0)?,
                    folder_id: row.get(1)?,
                    portable_path: row.get(2)?,
                    page_token: row.get(3)?,
                })
            },
        )
        .optional()?)
}

fn validate_frontier_path(path: &str) -> Result<()> {
    if path.is_empty() {
        Ok(())
    } else {
        validate_content_path(path)
    }
}

fn validate_scan_page_children(
    transaction: &Transaction<'_>,
    remote_root_id: &str,
    current: &FrontierRow,
    page: &ScanPage,
) -> Result<()> {
    let mut identities = BTreeSet::new();
    for entry in &page.entries {
        entry.validate()?;
        if entry.parent_id != current.folder_id || !identities.insert(entry.file_id.as_str()) {
            return Err(Error::InvalidRemoteEntry);
        }
        let relative = if current.portable_path.is_empty() {
            entry.path.as_str()
        } else {
            entry
                .path
                .strip_prefix(&current.portable_path)
                .and_then(|value| value.strip_prefix('/'))
                .ok_or(Error::InvalidRemoteEntry)?
        };
        if relative.is_empty() || relative.contains('/') {
            return Err(Error::InvalidRemoteEntry);
        }
        if entry.kind == RemoteEntryKind::Folder {
            if entry.file_id == remote_root_id {
                return Err(Error::InvalidRemoteEntry);
            }
            let already_seen = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM remote_entries WHERE file_id = ?1)",
                [entry.file_id.as_str()],
                |row| row.get::<_, bool>(0),
            )?;
            if already_seen {
                return Err(Error::InvalidRemoteEntry);
            }
        }
    }
    Ok(())
}

fn enqueue_child_folders(
    transaction: &Transaction<'_>,
    current: &FrontierRow,
    entries: &[RemoteEntry],
) -> Result<()> {
    let mut next_sequence: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM scan_frontier",
        [],
        |row| row.get(0),
    )?;
    let mut frontier_count: usize = transaction
        .query_row("SELECT COUNT(*) FROM scan_frontier", [], |row| {
            row.get::<_, i64>(0)
        })?
        .try_into()
        .map_err(|_| Error::InvalidRemoteEntry)?;
    for entry in entries
        .iter()
        .filter(|entry| entry.kind == RemoteEntryKind::Folder)
    {
        if entry.file_id == current.folder_id {
            return Err(Error::InvalidRemoteEntry);
        }
        let existing: Option<String> = transaction
            .query_row(
                "SELECT portable_path FROM scan_frontier WHERE folder_id = ?1",
                [entry.file_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(path) = existing {
            if path != entry.path {
                return Err(Error::InvalidRemoteEntry);
            }
            continue;
        }
        next_sequence = next_sequence
            .checked_add(1)
            .ok_or(Error::InvalidRemoteEntry)?;
        frontier_count = frontier_count
            .checked_add(1)
            .ok_or(Error::InvalidRemoteEntry)?;
        if frontier_count > MAX_SCAN_FRONTIER_FOLDERS {
            return Err(Error::InvalidRemoteEntry);
        }
        transaction.execute(
            "INSERT INTO scan_frontier(sequence, folder_id, portable_path, page_token)
             VALUES (?1, ?2, ?3, NULL)",
            params![next_sequence, entry.file_id, entry.path],
        )?;
    }
    Ok(())
}

fn require_state(connection: &Connection, expected_vault_id: Uuid) -> Result<VaultSyncState> {
    load_state(connection, expected_vault_id)?.ok_or(Error::InvalidStateTransition)
}

fn upsert_remote_entry(transaction: &Transaction<'_>, entry: &RemoteEntry) -> Result<()> {
    entry.validate()?;
    transaction.execute(
        "INSERT INTO remote_entries(
            file_id, parent_id, portable_path, kind, content_hash_algorithm,
            content_hash, remote_revision
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(file_id) DO UPDATE SET
            parent_id = excluded.parent_id,
            portable_path = excluded.portable_path,
            kind = excluded.kind,
            content_hash_algorithm = excluded.content_hash_algorithm,
            content_hash = excluded.content_hash,
            remote_revision = excluded.remote_revision",
        params![
            entry.file_id,
            entry.parent_id,
            entry.path,
            match entry.kind {
                RemoteEntryKind::File => "file",
                RemoteEntryKind::Folder => "folder",
            },
            entry
                .content_hash
                .as_ref()
                .map(|hash| hash.algorithm.as_str()),
            entry.content_hash.as_ref().map(|hash| hash.hex.as_str()),
            entry.remote_revision
        ],
    )?;
    Ok(())
}

fn insert_job(transaction: &Transaction<'_>, job: &QueueJob) -> Result<()> {
    transaction.execute(
        "INSERT INTO sync_jobs(
            operation_id, kind, path, destination_path, remote_file_id,
            expected_local_revision, state, attempt_count, next_attempt_at_unix_ms,
            created_at_unix_ms, last_error_code
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            job.operation_id.to_string(),
            job.kind.as_str(),
            job.path,
            job.destination_path,
            job.remote_file_id,
            job.expected_local_revision,
            job.state.as_str(),
            i64::from(job.attempt_count),
            u64_to_i64(job.next_attempt_at_unix_ms)?,
            u64_to_i64(job.created_at_unix_ms)?,
            job.last_error_code
        ],
    )?;
    Ok(())
}

fn load_job(connection: &Connection, operation_id: Uuid) -> Result<Option<QueueJob>> {
    connection
        .query_row(
            "SELECT operation_id, kind, path, destination_path, remote_file_id,
                    expected_local_revision, state, attempt_count, next_attempt_at_unix_ms,
                    created_at_unix_ms, last_error_code
             FROM sync_jobs WHERE operation_id = ?1",
            [operation_id.to_string()],
            row_to_job,
        )
        .optional()?
        .map_or(Ok(None), |job| Ok(Some(job?)))
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<QueueJob>> {
    let operation_id = row.get::<_, String>(0)?;
    let kind = row.get::<_, String>(1)?;
    let path = row.get::<_, String>(2)?;
    let destination_path = row.get::<_, Option<String>>(3)?;
    let remote_file_id = row.get::<_, Option<String>>(4)?;
    let expected_local_revision = row.get::<_, Option<String>>(5)?;
    let state = row.get::<_, String>(6)?;
    let attempt_count = row.get::<_, i64>(7)?;
    let next_attempt_at_unix_ms = row.get::<_, i64>(8)?;
    let created_at_unix_ms = row.get::<_, i64>(9)?;
    let last_error_code = row.get::<_, Option<String>>(10)?;
    Ok((|| {
        let job = QueueJob {
            operation_id: parse_uuid(&operation_id)?,
            kind: QueueJobKind::parse(&kind)?,
            path,
            destination_path,
            remote_file_id,
            expected_local_revision,
            state: JobState::parse(&state)?,
            attempt_count: u32::try_from(attempt_count).map_err(|_| Error::InvalidSchema)?,
            next_attempt_at_unix_ms: u64::try_from(next_attempt_at_unix_ms)
                .map_err(|_| Error::InvalidSchema)?,
            created_at_unix_ms: u64::try_from(created_at_unix_ms)
                .map_err(|_| Error::InvalidSchema)?,
            last_error_code,
        };
        job.validate()?;
        Ok(job)
    })())
}

fn load_change_batch(connection: &Connection) -> Result<Option<ChangeBatch>> {
    let base = connection
        .query_row(
            "SELECT batch_id, expected_cursor, next_cursor FROM change_batch WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((batch_id, expected_cursor, next_cursor)) = base else {
        return Ok(None);
    };
    validate_remote_token(&expected_cursor)?;
    validate_remote_token(&next_cursor)?;
    let (declared, applying, committed): (i64, i64, i64) = connection.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN state = 'applying' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state = 'committed' THEN 1 ELSE 0 END), 0)
         FROM change_batch_mutations WHERE batch_id = ?1",
        [&batch_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(Some(ChangeBatch {
        batch_id: parse_uuid(&batch_id)?,
        expected_cursor,
        next_cursor,
        declared_mutations: u64::try_from(declared).map_err(|_| Error::InvalidSchema)?,
        applying_mutations: u64::try_from(applying).map_err(|_| Error::InvalidSchema)?,
        committed_mutations: u64::try_from(committed).map_err(|_| Error::InvalidSchema)?,
    }))
}

fn load_local_mutation_state(
    connection: &Connection,
    batch_id: Uuid,
    mutation_id: &str,
) -> Result<Option<LocalMutationState>> {
    let state = connection
        .query_row(
            "SELECT state FROM change_batch_mutations
             WHERE batch_id = ?1 AND mutation_id = ?2",
            params![batch_id.to_string(), mutation_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    state.map_or(Ok(None), |value| {
        Ok(Some(LocalMutationState::parse(&value)?))
    })
}

fn load_local_mutations(
    connection: &Connection,
    batch_id: Uuid,
) -> Result<Vec<LocalMutationStatus>> {
    let mut statement = connection.prepare(
        "SELECT mutation_id, state FROM change_batch_mutations
         WHERE batch_id = ?1 ORDER BY mutation_id",
    )?;
    let rows = statement.query_map([batch_id.to_string()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (mutation_id, state) = row?;
        validate_redacted_code(&mutation_id)?;
        Ok(LocalMutationStatus {
            mutation_id,
            state: LocalMutationState::parse(&state)?,
        })
    })
    .collect()
}

fn query_count(connection: &Connection, query: &str) -> Result<u64> {
    let count: i64 = connection.query_row(query, [], |row| row.get(0))?;
    u64::try_from(count).map_err(|_| Error::InvalidSchema)
}
