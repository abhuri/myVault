use crate::{
    parse_uuid, u64_to_i64, validate_content_path, validate_private_reference,
    validate_redacted_code, validate_remote_id, validate_remote_token, validate_revision,
    ChangesPage, Error, RemoteChange, RemoteContentHash, RemoteEntry, RemoteEntryKind,
    RemoteHashAlgorithm, Result, ScanPage, ScanRequest, SyncPhase, VerifiedRemoteBinding,
    MAX_SCAN_FRONTIER_FOLDERS,
};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use fs2::FileExt;
use myvault_private_fs as private_fs;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const ROOT_DIRECTORY: &str = "sync-state";
const VERSION_DIRECTORY: &str = "v1";
const VAULTS_DIRECTORY: &str = "vaults";
const LEASE_NAME: &str = "sync-operation.lock";
const DATABASE_NAME: &str = "myvault-sync.sqlite3";

type PersistedRemoteEntry = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
);
type PersistedRemoteBase = (Option<String>, Option<String>, Option<String>, Option<i64>);

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
const REMOTE_ENTRIES_SCHEMA_V4: &str = "CREATE TABLE remote_entries (
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
    base_content_hash TEXT,
    base_byte_length INTEGER CHECK (base_byte_length >= 0),
    CHECK ((base_local_revision IS NULL AND base_remote_revision IS NULL AND base_content_hash IS NULL AND base_byte_length IS NULL)
        OR (base_local_revision IS NOT NULL AND base_remote_revision IS NOT NULL AND base_content_hash IS NOT NULL AND base_byte_length IS NOT NULL))
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
const CHANGE_BATCH_MUTATIONS_SCHEMA_V3: &str = "CREATE TABLE change_batch_mutations (
    batch_id TEXT NOT NULL,
    mutation_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'applying', 'committed')),
    PRIMARY KEY (batch_id, mutation_id),
    FOREIGN KEY (batch_id) REFERENCES change_batch(batch_id) ON DELETE CASCADE
)";
const CHANGE_BATCH_MUTATIONS_SCHEMA: &str = "CREATE TABLE change_batch_mutations (
    batch_id TEXT NOT NULL,
    mutation_id TEXT NOT NULL,
    dependency_kind TEXT NOT NULL CHECK (dependency_kind IN ('mutation', 'merge_publication', 'conflict_copy_publication', 'base_publication', 'legacy_v3')),
    operation_id TEXT,
    committed_evidence_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('pending', 'applying', 'needs_reconcile', 'committed')),
    PRIMARY KEY (batch_id, mutation_id),
    FOREIGN KEY (batch_id) REFERENCES change_batch(batch_id) ON DELETE CASCADE,
    FOREIGN KEY (operation_id) REFERENCES mutation_intents(operation_id),
    FOREIGN KEY (committed_evidence_id) REFERENCES mutation_verification_evidence(evidence_id),
    CHECK ((dependency_kind = 'legacy_v3' AND operation_id IS NULL AND committed_evidence_id IS NULL) OR (dependency_kind != 'legacy_v3' AND operation_id IS NOT NULL))
)";
const TRANSFERS_SCHEMA: &str = "CREATE TABLE transfers (
    operation_id TEXT PRIMARY KEY NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('upload', 'download')),
    portable_path TEXT NOT NULL,
    remote_parent_id TEXT NOT NULL,
    remote_file_id TEXT,
    display_name TEXT NOT NULL,
    expected_local_revision TEXT,
    expected_remote_revision TEXT,
    sha256 TEXT NOT NULL,
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    mime_class TEXT NOT NULL CHECK (mime_class IN ('markdown', 'blob')),
    operation_marker TEXT NOT NULL UNIQUE,
    stage_reference TEXT,
    base_reference TEXT,
    phase TEXT NOT NULL CHECK (phase IN ('pending', 'running', 'retry_scheduled', 'auth_required', 'needs_reconcile', 'completed')),
    attempt_count INTEGER NOT NULL CHECK (attempt_count >= 0),
    next_attempt_at_unix_ms INTEGER NOT NULL CHECK (next_attempt_at_unix_ms >= 0),
    created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
    updated_at_unix_ms INTEGER NOT NULL CHECK (updated_at_unix_ms >= 0),
    last_error_code TEXT,
    verified_local_revision TEXT,
    verified_remote_revision TEXT
)";
const TRANSFERS_DUE_INDEX_SCHEMA: &str = "CREATE INDEX transfers_due_idx
    ON transfers(phase, next_attempt_at_unix_ms, created_at_unix_ms, operation_id)";
const TRANSFER_HISTORY_SCHEMA: &str = "CREATE TABLE transfer_history (
    event_id INTEGER PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    outcome_code TEXT NOT NULL,
    occurred_at_unix_ms INTEGER NOT NULL CHECK (occurred_at_unix_ms >= 0),
    FOREIGN KEY (operation_id) REFERENCES transfers(operation_id)
)";
const MUTATION_INTENTS_SCHEMA: &str = "CREATE TABLE mutation_intents (
    operation_id TEXT PRIMARY KEY NOT NULL,
    operation_kind TEXT NOT NULL CHECK (operation_kind IN ('local_publish', 'merge_publish', 'conflict_copy_publish', 'base_publish', 'remote_existing_blocked')),
    account_id TEXT,
    remote_root_id TEXT,
    remote_file_id TEXT,
    source_parent_id TEXT,
    destination_parent_id TEXT,
    local_object_id TEXT,
    source_path TEXT,
    destination_path TEXT,
    expected_local_revision TEXT,
    expected_remote_revision TEXT,
    base_reference TEXT,
    base_local_revision TEXT,
    base_remote_revision TEXT,
    base_sha256 TEXT,
    base_byte_length INTEGER CHECK (base_byte_length >= 0),
    expected_local_sha256 TEXT,
    expected_local_byte_length INTEGER CHECK (expected_local_byte_length >= 0),
    expected_remote_sha256 TEXT,
    expected_remote_byte_length INTEGER CHECK (expected_remote_byte_length >= 0),
    operation_marker TEXT NOT NULL UNIQUE,
    intent_fingerprint TEXT NOT NULL,
    registered_at_unix_ms INTEGER NOT NULL CHECK (registered_at_unix_ms >= 0),
    CHECK ((base_sha256 IS NULL AND base_byte_length IS NULL) OR (base_sha256 IS NOT NULL AND base_byte_length IS NOT NULL)),
    CHECK ((expected_local_sha256 IS NULL AND expected_local_byte_length IS NULL) OR (expected_local_sha256 IS NOT NULL AND expected_local_byte_length IS NOT NULL)),
    CHECK ((expected_remote_sha256 IS NULL AND expected_remote_byte_length IS NULL) OR (expected_remote_sha256 IS NOT NULL AND expected_remote_byte_length IS NOT NULL))
)";
const MUTATION_STATE_SCHEMA: &str = "CREATE TABLE mutation_state (
    operation_id TEXT PRIMARY KEY NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('intent_durable', 'running', 'retry_scheduled', 'needs_reconcile', 'completed')),
    attempt_number INTEGER NOT NULL CHECK (attempt_number >= 0),
    state_version INTEGER NOT NULL CHECK (state_version >= 0),
    disposition TEXT CHECK (disposition IN ('verified_applied', 'verified_not_applied', 'retry_safe', 'needs_reconcile')),
    next_attempt_at_unix_ms INTEGER CHECK (next_attempt_at_unix_ms >= 0),
    retry_mode TEXT CHECK (retry_mode IN ('restart_exact', 'resume_exact')),
    resume_reference TEXT,
    last_evidence_id TEXT,
    outcome_code TEXT,
    updated_at_unix_ms INTEGER NOT NULL CHECK (updated_at_unix_ms >= 0),
    FOREIGN KEY (operation_id) REFERENCES mutation_intents(operation_id),
    FOREIGN KEY (last_evidence_id) REFERENCES mutation_verification_evidence(evidence_id),
    CHECK ((phase = 'retry_scheduled') = (next_attempt_at_unix_ms IS NOT NULL)),
    CHECK ((retry_mode IS NULL AND resume_reference IS NULL) OR (retry_mode = 'restart_exact' AND resume_reference IS NULL) OR (retry_mode = 'resume_exact' AND resume_reference IS NOT NULL))
)";
const MUTATION_EVENTS_SCHEMA: &str = "CREATE TABLE mutation_events (
    event_id INTEGER PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL CHECK (attempt_number >= 0),
    state_version INTEGER NOT NULL CHECK (state_version >= 0),
    phase TEXT NOT NULL CHECK (phase IN ('intent_durable', 'running', 'retry_scheduled', 'needs_reconcile', 'completed')),
    disposition TEXT CHECK (disposition IN ('verified_applied', 'verified_not_applied', 'retry_safe', 'needs_reconcile')),
    evidence_id TEXT,
    outcome_code TEXT,
    occurred_at_unix_ms INTEGER NOT NULL CHECK (occurred_at_unix_ms >= 0),
    FOREIGN KEY (operation_id) REFERENCES mutation_intents(operation_id),
    FOREIGN KEY (evidence_id) REFERENCES mutation_verification_evidence(evidence_id)
)";
const MUTATION_VERIFICATION_EVIDENCE_SCHEMA: &str = "CREATE TABLE mutation_verification_evidence (
    evidence_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL CHECK (attempt_number >= 0),
    capture_phase TEXT NOT NULL CHECK (capture_phase IN ('preflight', 'post_verify', 'reconcile')),
    disposition TEXT NOT NULL CHECK (disposition IN ('verified_applied', 'verified_not_applied', 'retry_safe', 'needs_reconcile')),
    outcome_code TEXT,
    observed_account_id TEXT,
    observed_remote_root_id TEXT,
    observed_remote_file_id TEXT,
    observed_parent_id TEXT,
    observed_path TEXT,
    observed_local_revision TEXT,
    observed_remote_revision TEXT,
    observed_sha256 TEXT,
    observed_byte_length INTEGER CHECK (observed_byte_length >= 0),
    observed_operation_marker TEXT,
    forbidden_side_effect INTEGER NOT NULL CHECK (forbidden_side_effect IN (0, 1)),
    verified_received_byte_offset INTEGER CHECK (verified_received_byte_offset >= 0),
    resume_reference TEXT,
    evidence_fingerprint TEXT NOT NULL,
    captured_at_unix_ms INTEGER NOT NULL CHECK (captured_at_unix_ms >= 0),
    FOREIGN KEY (operation_id) REFERENCES mutation_intents(operation_id),
    CHECK ((observed_sha256 IS NULL AND observed_byte_length IS NULL) OR (observed_sha256 IS NOT NULL AND observed_byte_length IS NOT NULL))
)";
const CONFLICT_EVIDENCE_SCHEMA: &str = "CREATE TABLE conflict_evidence (
    conflict_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL,
    stable_cell_id TEXT NOT NULL,
    local_state_code TEXT NOT NULL,
    remote_state_code TEXT NOT NULL,
    content_class TEXT NOT NULL,
    lineage_state TEXT NOT NULL,
    classification_code TEXT NOT NULL,
    ambiguity_reason TEXT NOT NULL,
    evidence_sufficiency TEXT NOT NULL,
    conflict_copy_operation_id TEXT,
    base_evidence_id TEXT,
    local_evidence_id TEXT,
    remote_evidence_id TEXT,
    base_sha256 TEXT,
    base_byte_length INTEGER CHECK (base_byte_length >= 0),
    local_sha256 TEXT,
    local_byte_length INTEGER CHECK (local_byte_length >= 0),
    remote_sha256 TEXT,
    remote_byte_length INTEGER CHECK (remote_byte_length >= 0),
    naming_version TEXT NOT NULL,
    normalized_collision_key TEXT NOT NULL,
    target_parent_id TEXT NOT NULL,
    expected_conflict_copy_sha256 TEXT,
    expected_conflict_copy_byte_length INTEGER CHECK (expected_conflict_copy_byte_length >= 0),
    explanation_code TEXT,
    device_alias TEXT,
    evidence_fingerprint TEXT NOT NULL,
    captured_at_unix_ms INTEGER NOT NULL CHECK (captured_at_unix_ms >= 0),
    FOREIGN KEY (operation_id) REFERENCES mutation_intents(operation_id),
    FOREIGN KEY (conflict_copy_operation_id) REFERENCES mutation_intents(operation_id),
    FOREIGN KEY (base_evidence_id) REFERENCES mutation_verification_evidence(evidence_id),
    FOREIGN KEY (local_evidence_id) REFERENCES mutation_verification_evidence(evidence_id),
    FOREIGN KEY (remote_evidence_id) REFERENCES mutation_verification_evidence(evidence_id),
    CHECK ((base_sha256 IS NULL AND base_byte_length IS NULL) OR (base_sha256 IS NOT NULL AND base_byte_length IS NOT NULL)),
    CHECK ((local_sha256 IS NULL AND local_byte_length IS NULL) OR (local_sha256 IS NOT NULL AND local_byte_length IS NOT NULL)),
    CHECK ((remote_sha256 IS NULL AND remote_byte_length IS NULL) OR (remote_sha256 IS NOT NULL AND remote_byte_length IS NOT NULL)),
    CHECK ((expected_conflict_copy_sha256 IS NULL AND expected_conflict_copy_byte_length IS NULL) OR (expected_conflict_copy_sha256 IS NOT NULL AND expected_conflict_copy_byte_length IS NOT NULL))
)";
const MUTATION_STATE_CLAIM_INDEX_SCHEMA: &str = "CREATE INDEX mutation_state_claim_idx
    ON mutation_state(phase, next_attempt_at_unix_ms, operation_id)";
const MUTATION_EVENTS_OPERATION_ATTEMPT_INDEX_SCHEMA: &str =
    "CREATE INDEX mutation_events_operation_attempt_idx
    ON mutation_events(operation_id, attempt_number, event_id)";
const MUTATION_EVIDENCE_OPERATION_ATTEMPT_INDEX_SCHEMA: &str =
    "CREATE INDEX mutation_evidence_operation_attempt_idx
    ON mutation_verification_evidence(operation_id, attempt_number, evidence_id)";
const CONFLICT_EVIDENCE_STABLE_CELL_INDEX_SCHEMA: &str =
    "CREATE UNIQUE INDEX conflict_evidence_stable_cell_idx
    ON conflict_evidence(stable_cell_id, conflict_id)";
const CONFLICT_EVIDENCE_COPY_INDEX_SCHEMA: &str = "CREATE UNIQUE INDEX conflict_evidence_copy_idx
    ON conflict_evidence(conflict_copy_operation_id) WHERE conflict_copy_operation_id IS NOT NULL";
const MUTATION_INTENTS_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER mutation_intents_no_update
    BEFORE UPDATE ON mutation_intents BEGIN SELECT RAISE(ABORT, 'mutation_intents_immutable'); END";
const MUTATION_INTENTS_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER mutation_intents_no_delete
    BEFORE DELETE ON mutation_intents BEGIN SELECT RAISE(ABORT, 'mutation_intents_immutable'); END";
const MUTATION_EVENTS_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER mutation_events_no_update
    BEFORE UPDATE ON mutation_events BEGIN SELECT RAISE(ABORT, 'mutation_events_immutable'); END";
const MUTATION_EVENTS_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER mutation_events_no_delete
    BEFORE DELETE ON mutation_events BEGIN SELECT RAISE(ABORT, 'mutation_events_immutable'); END";
const MUTATION_EVIDENCE_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER mutation_evidence_no_update
    BEFORE UPDATE ON mutation_verification_evidence BEGIN SELECT RAISE(ABORT, 'mutation_evidence_immutable'); END";
const MUTATION_EVIDENCE_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER mutation_evidence_no_delete
    BEFORE DELETE ON mutation_verification_evidence BEGIN SELECT RAISE(ABORT, 'mutation_evidence_immutable'); END";
const CONFLICT_EVIDENCE_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER conflict_evidence_no_update
    BEFORE UPDATE ON conflict_evidence BEGIN SELECT RAISE(ABORT, 'conflict_evidence_immutable'); END";
const CONFLICT_EVIDENCE_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER conflict_evidence_no_delete
    BEFORE DELETE ON conflict_evidence BEGIN SELECT RAISE(ABORT, 'conflict_evidence_immutable'); END";

const SCHEMA_OBJECTS_V1: [(&str, &str, &str); 8] = [
    ("table", "vault_state", VAULT_STATE_SCHEMA_V1),
    ("table", "remote_entries", REMOTE_ENTRIES_SCHEMA_V4),
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
        CHANGE_BATCH_MUTATIONS_SCHEMA_V3,
    ),
];

const SCHEMA_OBJECTS_V2: [(&str, &str, &str); 10] = [
    ("table", "vault_state", VAULT_STATE_SCHEMA),
    ("table", "remote_entries", REMOTE_ENTRIES_SCHEMA_V4),
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
        CHANGE_BATCH_MUTATIONS_SCHEMA_V3,
    ),
];

const SCHEMA_OBJECTS_V3: [(&str, &str, &str); 13] = [
    ("table", "vault_state", VAULT_STATE_SCHEMA),
    ("table", "remote_entries", REMOTE_ENTRIES_SCHEMA_V4),
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
        CHANGE_BATCH_MUTATIONS_SCHEMA_V3,
    ),
    ("table", "transfers", TRANSFERS_SCHEMA),
    ("index", "transfers_due_idx", TRANSFERS_DUE_INDEX_SCHEMA),
    ("table", "transfer_history", TRANSFER_HISTORY_SCHEMA),
];

const SCHEMA_OBJECTS_V4: [(&str, &str, &str); 31] = [
    ("table", "vault_state", VAULT_STATE_SCHEMA),
    ("table", "remote_entries", REMOTE_ENTRIES_SCHEMA_V4),
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
    ("table", "mutation_intents", MUTATION_INTENTS_SCHEMA),
    (
        "table",
        "mutation_verification_evidence",
        MUTATION_VERIFICATION_EVIDENCE_SCHEMA,
    ),
    ("table", "mutation_state", MUTATION_STATE_SCHEMA),
    ("table", "mutation_events", MUTATION_EVENTS_SCHEMA),
    ("table", "conflict_evidence", CONFLICT_EVIDENCE_SCHEMA),
    (
        "table",
        "change_batch_mutations",
        CHANGE_BATCH_MUTATIONS_SCHEMA,
    ),
    ("table", "transfers", TRANSFERS_SCHEMA),
    ("index", "transfers_due_idx", TRANSFERS_DUE_INDEX_SCHEMA),
    ("table", "transfer_history", TRANSFER_HISTORY_SCHEMA),
    (
        "index",
        "mutation_state_claim_idx",
        MUTATION_STATE_CLAIM_INDEX_SCHEMA,
    ),
    (
        "index",
        "mutation_events_operation_attempt_idx",
        MUTATION_EVENTS_OPERATION_ATTEMPT_INDEX_SCHEMA,
    ),
    (
        "index",
        "mutation_evidence_operation_attempt_idx",
        MUTATION_EVIDENCE_OPERATION_ATTEMPT_INDEX_SCHEMA,
    ),
    (
        "index",
        "conflict_evidence_stable_cell_idx",
        CONFLICT_EVIDENCE_STABLE_CELL_INDEX_SCHEMA,
    ),
    (
        "index",
        "conflict_evidence_copy_idx",
        CONFLICT_EVIDENCE_COPY_INDEX_SCHEMA,
    ),
    (
        "trigger",
        "mutation_intents_no_update",
        MUTATION_INTENTS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_intents_no_delete",
        MUTATION_INTENTS_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_events_no_update",
        MUTATION_EVENTS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_events_no_delete",
        MUTATION_EVENTS_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_evidence_no_update",
        MUTATION_EVIDENCE_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_evidence_no_delete",
        MUTATION_EVIDENCE_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "conflict_evidence_no_update",
        CONFLICT_EVIDENCE_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "conflict_evidence_no_delete",
        CONFLICT_EVIDENCE_NO_DELETE_TRIGGER,
    ),
];

const SCHEMA_OBJECTS: [(&str, &str, &str); 31] = [
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
    ("table", "mutation_intents", MUTATION_INTENTS_SCHEMA),
    (
        "table",
        "mutation_verification_evidence",
        MUTATION_VERIFICATION_EVIDENCE_SCHEMA,
    ),
    ("table", "mutation_state", MUTATION_STATE_SCHEMA),
    ("table", "mutation_events", MUTATION_EVENTS_SCHEMA),
    ("table", "conflict_evidence", CONFLICT_EVIDENCE_SCHEMA),
    (
        "table",
        "change_batch_mutations",
        CHANGE_BATCH_MUTATIONS_SCHEMA,
    ),
    ("table", "transfers", TRANSFERS_SCHEMA),
    ("index", "transfers_due_idx", TRANSFERS_DUE_INDEX_SCHEMA),
    ("table", "transfer_history", TRANSFER_HISTORY_SCHEMA),
    (
        "index",
        "mutation_state_claim_idx",
        MUTATION_STATE_CLAIM_INDEX_SCHEMA,
    ),
    (
        "index",
        "mutation_events_operation_attempt_idx",
        MUTATION_EVENTS_OPERATION_ATTEMPT_INDEX_SCHEMA,
    ),
    (
        "index",
        "mutation_evidence_operation_attempt_idx",
        MUTATION_EVIDENCE_OPERATION_ATTEMPT_INDEX_SCHEMA,
    ),
    (
        "index",
        "conflict_evidence_stable_cell_idx",
        CONFLICT_EVIDENCE_STABLE_CELL_INDEX_SCHEMA,
    ),
    (
        "index",
        "conflict_evidence_copy_idx",
        CONFLICT_EVIDENCE_COPY_INDEX_SCHEMA,
    ),
    (
        "trigger",
        "mutation_intents_no_update",
        MUTATION_INTENTS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_intents_no_delete",
        MUTATION_INTENTS_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_events_no_update",
        MUTATION_EVENTS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_events_no_delete",
        MUTATION_EVENTS_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_evidence_no_update",
        MUTATION_EVIDENCE_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_evidence_no_delete",
        MUTATION_EVIDENCE_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "conflict_evidence_no_update",
        CONFLICT_EVIDENCE_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "conflict_evidence_no_delete",
        CONFLICT_EVIDENCE_NO_DELETE_TRIGGER,
    ),
];

pub const SCHEMA_VERSION: i64 = 5;
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

/// Exact three-way base evidence for one remote file, when a verified transfer
/// has established the same bytes on both sides.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteBaseEvidence {
    pub local_revision: String,
    pub remote_revision: String,
    pub content_hash: String,
    pub byte_length: u64,
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

/// Direction of one content-bearing R2 transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferDirection {
    Upload,
    Download,
}

impl TransferDirection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "upload" => Ok(Self::Upload),
            "download" => Ok(Self::Download),
            _ => Err(Error::InvalidSchema),
        }
    }
}

/// Bounded content classification; raw provider MIME strings are not durable evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferMimeClass {
    Markdown,
    Blob,
}

impl TransferMimeClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Blob => "blob",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "markdown" => Ok(Self::Markdown),
            "blob" => Ok(Self::Blob),
            _ => Err(Error::InvalidSchema),
        }
    }
}

/// Durable transfer phase. `Running` is always recovered to `NeedsReconcile` after restart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferPhase {
    Pending,
    Running,
    RetryScheduled,
    AuthRequired,
    NeedsReconcile,
    Completed,
}

impl TransferPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::RetryScheduled => "retry_scheduled",
            Self::AuthRequired => "auth_required",
            Self::NeedsReconcile => "needs_reconcile",
            Self::Completed => "completed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "retry_scheduled" => Ok(Self::RetryScheduled),
            "auth_required" => Ok(Self::AuthRequired),
            "needs_reconcile" => Ok(Self::NeedsReconcile),
            "completed" => Ok(Self::Completed),
            _ => Err(Error::InvalidSchema),
        }
    }
}

/// Complete non-secret evidence required before a transfer side effect may start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferRecord {
    pub operation_id: Uuid,
    pub direction: TransferDirection,
    pub portable_path: String,
    pub remote_parent_id: String,
    pub remote_file_id: Option<String>,
    pub display_name: String,
    pub expected_local_revision: Option<String>,
    pub expected_remote_revision: Option<String>,
    pub sha256: String,
    pub byte_length: u64,
    pub mime_class: TransferMimeClass,
    pub operation_marker: String,
    pub stage_reference: Option<String>,
    pub base_reference: Option<String>,
    pub phase: TransferPhase,
    pub attempt_count: u32,
    pub next_attempt_at_unix_ms: u64,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub last_error_code: Option<String>,
    pub verified_local_revision: Option<String>,
    pub verified_remote_revision: Option<String>,
}

impl TransferRecord {
    /// Creates validated pending evidence without credentials, bodies, URLs, or ambient paths.
    ///
    /// # Errors
    /// Rejects malformed identities, revisions, digest, timestamps, or opaque references.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation_id: Uuid,
        direction: TransferDirection,
        portable_path: impl Into<String>,
        remote_parent_id: impl Into<String>,
        remote_file_id: Option<String>,
        expected_local_revision: Option<String>,
        expected_remote_revision: Option<String>,
        sha256: impl Into<String>,
        byte_length: u64,
        mime_class: TransferMimeClass,
        operation_marker: impl Into<String>,
        stage_reference: Option<String>,
        base_reference: Option<String>,
        created_at_unix_ms: u64,
    ) -> Result<Self> {
        let portable_path = portable_path.into();
        let display_name = portable_path
            .rsplit('/')
            .next()
            .ok_or(Error::InvalidTransferEvidence)?
            .to_owned();
        let record = Self {
            operation_id,
            direction,
            portable_path,
            remote_parent_id: remote_parent_id.into(),
            remote_file_id,
            display_name,
            expected_local_revision,
            expected_remote_revision,
            sha256: sha256.into(),
            byte_length,
            mime_class,
            operation_marker: operation_marker.into(),
            stage_reference,
            base_reference,
            phase: TransferPhase::Pending,
            attempt_count: 0,
            next_attempt_at_unix_ms: created_at_unix_ms,
            created_at_unix_ms,
            updated_at_unix_ms: created_at_unix_ms,
            last_error_code: None,
            verified_local_revision: None,
            verified_remote_revision: None,
        };
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<()> {
        if self.operation_id.is_nil() {
            return Err(Error::InvalidTransferEvidence);
        }
        validate_content_path(&self.portable_path)?;
        if self.display_name
            != self
                .portable_path
                .rsplit('/')
                .next()
                .ok_or(Error::InvalidTransferEvidence)?
        {
            return Err(Error::InvalidTransferEvidence);
        }
        validate_remote_id(&self.remote_parent_id)?;
        if let Some(value) = &self.remote_file_id {
            validate_remote_id(value)?;
        }
        if self.direction == TransferDirection::Download && self.remote_file_id.is_none() {
            return Err(Error::InvalidTransferEvidence);
        }
        if self.direction == TransferDirection::Upload
            && (self.expected_local_revision.is_none() || self.stage_reference.is_none())
        {
            return Err(Error::InvalidTransferEvidence);
        }
        if let Some(value) = &self.expected_local_revision {
            validate_revision(value)?;
        }
        if let Some(value) = &self.expected_remote_revision {
            validate_remote_id(value)?;
        }
        validate_revision(&self.sha256)?;
        validate_remote_id(&self.operation_marker)?;
        for reference in [
            self.stage_reference.as_deref(),
            self.base_reference.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_private_reference(reference)?;
        }
        if let Some(value) = &self.last_error_code {
            validate_redacted_code(value)?;
        }
        if let Some(value) = &self.verified_local_revision {
            validate_revision(value)?;
        }
        if let Some(value) = &self.verified_remote_revision {
            validate_remote_id(value)?;
        }
        u64_to_i64(self.byte_length)?;
        u64_to_i64(self.next_attempt_at_unix_ms)?;
        u64_to_i64(self.created_at_unix_ms)?;
        u64_to_i64(self.updated_at_unix_ms)?;
        if self.updated_at_unix_ms < self.created_at_unix_ms
            || (self.phase == TransferPhase::Completed
                && (self.remote_file_id.is_none()
                    || self.base_reference.is_none()
                    || self.verified_local_revision.is_none()
                    || self.verified_remote_revision.is_none()))
        {
            return Err(Error::InvalidTransferEvidence);
        }
        Ok(())
    }

    fn same_registration(&self, other: &Self) -> bool {
        self.operation_id == other.operation_id
            && self.direction == other.direction
            && self.portable_path == other.portable_path
            && self.remote_parent_id == other.remote_parent_id
            && (other.remote_file_id.is_none() || self.remote_file_id == other.remote_file_id)
            && self.display_name == other.display_name
            && self.expected_local_revision == other.expected_local_revision
            && self.expected_remote_revision == other.expected_remote_revision
            && self.sha256 == other.sha256
            && self.byte_length == other.byte_length
            && self.mime_class == other.mime_class
            && self.operation_marker == other.operation_marker
            && self.stage_reference == other.stage_reference
            && (other.base_reference.is_none() || self.base_reference == other.base_reference)
            && self.created_at_unix_ms == other.created_at_unix_ms
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferRegistrationOutcome {
    Registered,
    AlreadyPresent,
    AlreadyCompleted,
}

/// Exact identities proven after content transfer and byte verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferCompletion {
    pub remote_file_id: String,
    pub remote_revision: String,
    pub local_revision: String,
    pub base_reference: String,
    pub outcome_code: String,
    pub occurred_at_unix_ms: u64,
}

impl TransferCompletion {
    /// Creates exact verified completion evidence.
    ///
    /// # Errors
    /// Rejects malformed identities, revisions, references, codes, or timestamps.
    pub fn new(
        remote_file_id: impl Into<String>,
        remote_revision: impl Into<String>,
        local_revision: impl Into<String>,
        base_reference: impl Into<String>,
        outcome_code: impl Into<String>,
        occurred_at_unix_ms: u64,
    ) -> Result<Self> {
        let completion = Self {
            remote_file_id: remote_file_id.into(),
            remote_revision: remote_revision.into(),
            local_revision: local_revision.into(),
            base_reference: base_reference.into(),
            outcome_code: outcome_code.into(),
            occurred_at_unix_ms,
        };
        completion.validate()?;
        Ok(completion)
    }

    fn validate(&self) -> Result<()> {
        validate_remote_id(&self.remote_file_id)?;
        validate_remote_id(&self.remote_revision)?;
        validate_revision(&self.local_revision)?;
        validate_private_reference(&self.base_reference)?;
        validate_redacted_code(&self.outcome_code)?;
        u64_to_i64(self.occurred_at_unix_ms)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferCompletionOutcome {
    Completed,
    AlreadyCompleted,
}

/// Redacted durable transfer counts safe to expose through native status DTOs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransferSummary {
    pub pending: u64,
    pub running: u64,
    pub retry_scheduled: u64,
    pub auth_required: u64,
    pub needs_reconcile: u64,
    pub completed: u64,
}

impl TransferSummary {
    #[must_use]
    pub const fn active(self) -> u64 {
        self.pending
            + self.running
            + self.retry_scheduled
            + self.auth_required
            + self.needs_reconcile
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalMutationState {
    Pending,
    Applying,
    NeedsReconcile,
    Committed,
}

impl LocalMutationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applying => "applying",
            Self::NeedsReconcile => "needs_reconcile",
            Self::Committed => "committed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "applying" => Ok(Self::Applying),
            "needs_reconcile" => Ok(Self::NeedsReconcile),
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

/// Typed R3 cursor dependency; this is not a provider capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeBatchDependencyKind {
    Mutation,
    MergePublication,
    ConflictCopyPublication,
    BasePublication,
}

impl ChangeBatchDependencyKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Mutation => "mutation",
            Self::MergePublication => "merge_publication",
            Self::ConflictCopyPublication => "conflict_copy_publication",
            Self::BasePublication => "base_publication",
        }
    }

    const fn required_operation_kind(self) -> MutationOperationKind {
        match self {
            Self::Mutation => MutationOperationKind::LocalPublish,
            Self::MergePublication => MutationOperationKind::MergePublish,
            Self::ConflictCopyPublication => MutationOperationKind::ConflictCopyPublish,
            Self::BasePublication => MutationOperationKind::BasePublish,
        }
    }
}

/// One immutable operation required before a typed R3 cursor batch may commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangeBatchDependency {
    pub operation_id: Uuid,
    pub kind: ChangeBatchDependencyKind,
}

/// Immutable R3 mutation kind; this enum does not grant provider capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationOperationKind {
    LocalPublish,
    MergePublish,
    ConflictCopyPublish,
    BasePublish,
    RemoteExistingBlocked,
}

impl MutationOperationKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::LocalPublish => "local_publish",
            Self::MergePublish => "merge_publish",
            Self::ConflictCopyPublish => "conflict_copy_publish",
            Self::BasePublish => "base_publish",
            Self::RemoteExistingBlocked => "remote_existing_blocked",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "local_publish" => Ok(Self::LocalPublish),
            "merge_publish" => Ok(Self::MergePublish),
            "conflict_copy_publish" => Ok(Self::ConflictCopyPublish),
            "base_publish" => Ok(Self::BasePublish),
            "remote_existing_blocked" => Ok(Self::RemoteExistingBlocked),
            _ => Err(Error::InvalidSchema),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationPhase {
    IntentDurable,
    Running,
    RetryScheduled,
    NeedsReconcile,
    Completed,
}

impl MutationPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::IntentDurable => "intent_durable",
            Self::Running => "running",
            Self::RetryScheduled => "retry_scheduled",
            Self::NeedsReconcile => "needs_reconcile",
            Self::Completed => "completed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "intent_durable" => Ok(Self::IntentDurable),
            "running" => Ok(Self::Running),
            "retry_scheduled" => Ok(Self::RetryScheduled),
            "needs_reconcile" => Ok(Self::NeedsReconcile),
            "completed" => Ok(Self::Completed),
            _ => Err(Error::InvalidSchema),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationDisposition {
    VerifiedApplied,
    VerifiedNotApplied,
    RetrySafe,
    NeedsReconcile,
}

impl MutationDisposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::VerifiedApplied => "verified_applied",
            Self::VerifiedNotApplied => "verified_not_applied",
            Self::RetrySafe => "retry_safe",
            Self::NeedsReconcile => "needs_reconcile",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "verified_applied" => Ok(Self::VerifiedApplied),
            "verified_not_applied" => Ok(Self::VerifiedNotApplied),
            "retry_safe" => Ok(Self::RetrySafe),
            "needs_reconcile" => Ok(Self::NeedsReconcile),
            _ => Err(Error::InvalidSchema),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationRetryMode {
    RestartExact,
    ResumeExact,
}

impl MutationRetryMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RestartExact => "restart_exact",
            Self::ResumeExact => "resume_exact",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "restart_exact" => Ok(Self::RestartExact),
            "resume_exact" => Ok(Self::ResumeExact),
            _ => Err(Error::InvalidSchema),
        }
    }
}

/// Immutable fields that identify one R3 operation without provider credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationIntent {
    pub operation_id: Uuid,
    pub operation_kind: MutationOperationKind,
    pub account_id: Option<String>,
    pub remote_root_id: Option<String>,
    pub remote_file_id: Option<String>,
    pub source_parent_id: Option<String>,
    pub destination_parent_id: Option<String>,
    pub local_object_id: Option<String>,
    pub source_path: Option<String>,
    pub destination_path: Option<String>,
    pub expected_local_revision: Option<String>,
    pub expected_remote_revision: Option<String>,
    pub base_reference: Option<String>,
    pub base_local_revision: Option<String>,
    pub base_remote_revision: Option<String>,
    pub base_sha256: Option<String>,
    pub base_byte_length: Option<u64>,
    pub expected_local_sha256: Option<String>,
    pub expected_local_byte_length: Option<u64>,
    pub expected_remote_sha256: Option<String>,
    pub expected_remote_byte_length: Option<u64>,
    pub operation_marker: String,
    pub intent_fingerprint: String,
    pub registered_at_unix_ms: u64,
}

impl MutationIntent {
    /// Returns the engine-defined fingerprint for immutable correctness fields.
    #[must_use]
    pub fn canonical_fingerprint(&self) -> String {
        canonical_fingerprint(
            "r3-mutation-intent-v1",
            [
                (
                    "operation_kind",
                    Some(self.operation_kind.as_str().to_owned()),
                ),
                ("account_id", self.account_id.clone()),
                ("remote_root_id", self.remote_root_id.clone()),
                ("remote_file_id", self.remote_file_id.clone()),
                ("source_parent_id", self.source_parent_id.clone()),
                ("destination_parent_id", self.destination_parent_id.clone()),
                ("local_object_id", self.local_object_id.clone()),
                ("source_path", self.source_path.clone()),
                ("destination_path", self.destination_path.clone()),
                (
                    "expected_local_revision",
                    self.expected_local_revision.clone(),
                ),
                (
                    "expected_remote_revision",
                    self.expected_remote_revision.clone(),
                ),
                ("base_reference", self.base_reference.clone()),
                ("base_local_revision", self.base_local_revision.clone()),
                ("base_remote_revision", self.base_remote_revision.clone()),
                ("base_sha256", self.base_sha256.clone()),
                (
                    "base_byte_length",
                    self.base_byte_length.map(|value| value.to_string()),
                ),
                ("expected_local_sha256", self.expected_local_sha256.clone()),
                (
                    "expected_local_byte_length",
                    self.expected_local_byte_length
                        .map(|value| value.to_string()),
                ),
                (
                    "expected_remote_sha256",
                    self.expected_remote_sha256.clone(),
                ),
                (
                    "expected_remote_byte_length",
                    self.expected_remote_byte_length
                        .map(|value| value.to_string()),
                ),
                ("operation_marker", Some(self.operation_marker.clone())),
            ],
        )
    }

    /// Builds the one allowed representation of a detected attempt to mutate an
    /// already-existing remote item. The returned intent is deliberately
    /// non-executable and comes with its exact initial `NeedsReconcile`
    /// evidence; callers must register both together.
    ///
    /// This is a state/evidence constructor only. It does not grant any
    /// provider mutation capability.
    ///
    /// # Errors
    /// Returns validation errors when the supplied identity, revisions, hashes,
    /// or timestamp are not suitable for durable evidence.
    pub fn remote_existing_blocked(
        operation_id: Uuid,
        input: RemoteExistingBlockedInput,
        registered_at_unix_ms: u64,
    ) -> Result<(Self, MutationVerificationEvidence)> {
        let operation_marker = format!("r3-blocked-{}", operation_id.simple());
        let mut intent = Self {
            operation_id,
            operation_kind: MutationOperationKind::RemoteExistingBlocked,
            account_id: Some(input.account_id.clone()),
            remote_root_id: Some(input.remote_root_id.clone()),
            remote_file_id: Some(input.remote_file_id.clone()),
            source_parent_id: Some(input.source_parent_id.clone()),
            destination_parent_id: None,
            local_object_id: input.local_object_id,
            source_path: Some(input.source_path.clone()),
            destination_path: None,
            expected_local_revision: Some(input.expected_local_revision.clone()),
            expected_remote_revision: Some(input.expected_remote_revision.clone()),
            base_reference: input.base_reference,
            base_local_revision: input.base_local_revision,
            base_remote_revision: input.base_remote_revision,
            base_sha256: input.base_sha256,
            base_byte_length: input.base_byte_length,
            expected_local_sha256: Some(input.expected_local_sha256.clone()),
            expected_local_byte_length: Some(input.expected_local_byte_length),
            expected_remote_sha256: input.expected_remote_sha256.clone(),
            expected_remote_byte_length: input.expected_remote_byte_length,
            operation_marker,
            intent_fingerprint: String::new(),
            registered_at_unix_ms,
        };
        intent.intent_fingerprint = intent.canonical_fingerprint();
        validate_mutation_intent(&intent)?;

        let evidence_id = Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!(
                "myvault-r3-remote-existing-blocked-evidence\\0{}\\0{}",
                operation_id, intent.intent_fingerprint
            )
            .as_bytes(),
        );
        let mut evidence = MutationVerificationEvidence {
            evidence_id,
            operation_id,
            attempt_number: 0,
            capture_phase: MutationEvidenceCapturePhase::Preflight,
            disposition: MutationDisposition::NeedsReconcile,
            outcome_code: Some("remote_existing_blocked".into()),
            observed_account_id: Some(input.account_id),
            observed_remote_root_id: Some(input.remote_root_id),
            observed_remote_file_id: Some(input.remote_file_id),
            observed_parent_id: Some(input.source_parent_id),
            observed_path: Some(input.source_path),
            observed_local_revision: Some(input.expected_local_revision),
            observed_remote_revision: Some(input.expected_remote_revision),
            observed_sha256: input.expected_remote_sha256,
            observed_byte_length: input.expected_remote_byte_length,
            observed_operation_marker: Some(intent.operation_marker.clone()),
            forbidden_side_effect: true,
            verified_received_byte_offset: None,
            resume_reference: None,
            evidence_fingerprint: String::new(),
            captured_at_unix_ms: registered_at_unix_ms,
        };
        evidence.evidence_fingerprint = evidence.canonical_fingerprint();
        validate_mutation_evidence(&evidence)?;
        Ok((intent, evidence))
    }
}

/// Exact facts captured when an existing remote item is rejected before a
/// provider mutation can be attempted. Optional base fields must be supplied
/// together when a verified three-way base is available.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteExistingBlockedInput {
    pub account_id: String,
    pub remote_root_id: String,
    pub remote_file_id: String,
    pub source_parent_id: String,
    pub source_path: String,
    pub local_object_id: Option<String>,
    pub expected_local_revision: String,
    pub expected_local_sha256: String,
    pub expected_local_byte_length: u64,
    pub expected_remote_revision: String,
    pub expected_remote_sha256: Option<String>,
    pub expected_remote_byte_length: Option<u64>,
    pub base_reference: Option<String>,
    pub base_local_revision: Option<String>,
    pub base_remote_revision: Option<String>,
    pub base_sha256: Option<String>,
    pub base_byte_length: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationState {
    pub operation_id: Uuid,
    pub phase: MutationPhase,
    pub attempt_number: u32,
    pub state_version: u64,
    pub disposition: Option<MutationDisposition>,
    pub next_attempt_at_unix_ms: Option<u64>,
    pub retry_mode: Option<MutationRetryMode>,
    pub resume_reference: Option<String>,
    pub last_evidence_id: Option<Uuid>,
    pub outcome_code: Option<String>,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationVerificationEvidence {
    pub evidence_id: Uuid,
    pub operation_id: Uuid,
    pub attempt_number: u32,
    pub capture_phase: MutationEvidenceCapturePhase,
    pub disposition: MutationDisposition,
    pub outcome_code: Option<String>,
    pub observed_account_id: Option<String>,
    pub observed_remote_root_id: Option<String>,
    pub observed_remote_file_id: Option<String>,
    pub observed_parent_id: Option<String>,
    pub observed_path: Option<String>,
    pub observed_local_revision: Option<String>,
    pub observed_remote_revision: Option<String>,
    pub observed_sha256: Option<String>,
    pub observed_byte_length: Option<u64>,
    pub observed_operation_marker: Option<String>,
    pub forbidden_side_effect: bool,
    pub verified_received_byte_offset: Option<u64>,
    pub resume_reference: Option<String>,
    pub evidence_fingerprint: String,
    pub captured_at_unix_ms: u64,
}

impl MutationVerificationEvidence {
    /// Returns the engine-defined fingerprint excluding explanatory capture time.
    #[must_use]
    pub fn canonical_fingerprint(&self) -> String {
        canonical_fingerprint(
            "r3-mutation-evidence-v1",
            [
                ("operation_id", Some(self.operation_id.to_string())),
                ("attempt_number", Some(self.attempt_number.to_string())),
                (
                    "capture_phase",
                    Some(self.capture_phase.as_str().to_owned()),
                ),
                ("disposition", Some(self.disposition.as_str().to_owned())),
                ("outcome_code", self.outcome_code.clone()),
                ("observed_account_id", self.observed_account_id.clone()),
                (
                    "observed_remote_root_id",
                    self.observed_remote_root_id.clone(),
                ),
                (
                    "observed_remote_file_id",
                    self.observed_remote_file_id.clone(),
                ),
                ("observed_parent_id", self.observed_parent_id.clone()),
                ("observed_path", self.observed_path.clone()),
                (
                    "observed_local_revision",
                    self.observed_local_revision.clone(),
                ),
                (
                    "observed_remote_revision",
                    self.observed_remote_revision.clone(),
                ),
                ("observed_sha256", self.observed_sha256.clone()),
                (
                    "observed_byte_length",
                    self.observed_byte_length.map(|value| value.to_string()),
                ),
                (
                    "observed_operation_marker",
                    self.observed_operation_marker.clone(),
                ),
                (
                    "forbidden_side_effect",
                    Some(u8::from(self.forbidden_side_effect).to_string()),
                ),
                (
                    "verified_received_byte_offset",
                    self.verified_received_byte_offset
                        .map(|value| value.to_string()),
                ),
                ("resume_reference", self.resume_reference.clone()),
            ],
        )
    }
}

/// Immutable conflict-classification envelope; R3.1 persists it but does not classify content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictEvidence {
    pub conflict_id: String,
    pub operation_id: Uuid,
    pub stable_cell_id: String,
    pub local_state_code: String,
    pub remote_state_code: String,
    pub content_class: String,
    pub lineage_state: String,
    pub classification_code: String,
    pub ambiguity_reason: String,
    pub evidence_sufficiency: String,
    pub conflict_copy_operation_id: Option<Uuid>,
    pub base_evidence_id: Option<Uuid>,
    pub local_evidence_id: Option<Uuid>,
    pub remote_evidence_id: Option<Uuid>,
    pub base_sha256: Option<String>,
    pub base_byte_length: Option<u64>,
    pub local_sha256: Option<String>,
    pub local_byte_length: Option<u64>,
    pub remote_sha256: Option<String>,
    pub remote_byte_length: Option<u64>,
    pub naming_version: String,
    pub normalized_collision_key: String,
    pub target_parent_id: String,
    pub expected_conflict_copy_sha256: Option<String>,
    pub expected_conflict_copy_byte_length: Option<u64>,
    pub explanation_code: Option<String>,
    pub device_alias: Option<String>,
    pub evidence_fingerprint: String,
    pub captured_at_unix_ms: u64,
}

impl ConflictEvidence {
    /// Returns the engine-defined fingerprint excluding explanatory device/time metadata.
    #[must_use]
    pub fn canonical_fingerprint(&self) -> String {
        canonical_fingerprint(
            "r3-conflict-evidence-v1",
            [
                ("conflict_id", Some(self.conflict_id.clone())),
                ("operation_id", Some(self.operation_id.to_string())),
                ("stable_cell_id", Some(self.stable_cell_id.clone())),
                ("local_state_code", Some(self.local_state_code.clone())),
                ("remote_state_code", Some(self.remote_state_code.clone())),
                ("content_class", Some(self.content_class.clone())),
                ("lineage_state", Some(self.lineage_state.clone())),
                (
                    "classification_code",
                    Some(self.classification_code.clone()),
                ),
                ("ambiguity_reason", Some(self.ambiguity_reason.clone())),
                (
                    "evidence_sufficiency",
                    Some(self.evidence_sufficiency.clone()),
                ),
                (
                    "conflict_copy_operation_id",
                    self.conflict_copy_operation_id
                        .map(|value| value.to_string()),
                ),
                (
                    "base_evidence_id",
                    self.base_evidence_id.map(|value| value.to_string()),
                ),
                (
                    "local_evidence_id",
                    self.local_evidence_id.map(|value| value.to_string()),
                ),
                (
                    "remote_evidence_id",
                    self.remote_evidence_id.map(|value| value.to_string()),
                ),
                ("base_sha256", self.base_sha256.clone()),
                (
                    "base_byte_length",
                    self.base_byte_length.map(|value| value.to_string()),
                ),
                ("local_sha256", self.local_sha256.clone()),
                (
                    "local_byte_length",
                    self.local_byte_length.map(|value| value.to_string()),
                ),
                ("remote_sha256", self.remote_sha256.clone()),
                (
                    "remote_byte_length",
                    self.remote_byte_length.map(|value| value.to_string()),
                ),
                ("naming_version", Some(self.naming_version.clone())),
                (
                    "normalized_collision_key",
                    Some(self.normalized_collision_key.clone()),
                ),
                ("target_parent_id", Some(self.target_parent_id.clone())),
                (
                    "expected_conflict_copy_sha256",
                    self.expected_conflict_copy_sha256.clone(),
                ),
                (
                    "expected_conflict_copy_byte_length",
                    self.expected_conflict_copy_byte_length
                        .map(|value| value.to_string()),
                ),
                ("explanation_code", self.explanation_code.clone()),
            ],
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictEvidenceRegistrationOutcome {
    Registered,
    AlreadyPresent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationEvidenceCapturePhase {
    Preflight,
    PostVerify,
    Reconcile,
}

impl MutationEvidenceCapturePhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::PostVerify => "post_verify",
            Self::Reconcile => "reconcile",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationEvent {
    pub event_id: u64,
    pub operation_id: Uuid,
    pub attempt_number: u32,
    pub state_version: u64,
    pub phase: MutationPhase,
    pub disposition: Option<MutationDisposition>,
    pub evidence_id: Option<Uuid>,
    pub outcome_code: Option<String>,
    pub occurred_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationRegistrationOutcome {
    Registered,
    AlreadyPresent,
    AlreadyCompleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationOutcomeTransition {
    VerifiedApplied,
    /// Reserved until an approved executor can revalidate exact retry preconditions.
    VerifiedNotApplied {
        next_attempt_at_unix_ms: u64,
    },
    /// Reserved until an approved executor can revalidate exact resumable state.
    RetrySafe {
        next_attempt_at_unix_ms: u64,
        resume_reference: String,
    },
    NeedsReconcile,
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

#[derive(Clone, Copy)]
enum PrivateStoragePolicy {
    Standard,
    #[cfg(target_os = "android")]
    NativeAndroidNoBackup,
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
        let root = private_fs::open_private_disjoint_held_root(app_data_root, vault_root)?;
        Self::open_from_held_private_root(&root, vault_id)
    }

    /// Opens from an already validated held private-root capability.
    ///
    /// This native-only integration API accepts no arbitrary path and
    /// revalidates the retained canonical/held identity before and after the
    /// SQLite ambient-path open.
    ///
    /// # Errors
    /// Fails closed for stale capabilities, unsafe storage, invalid IDs,
    /// corrupt evidence, lease contention, or migration failures.
    #[doc(hidden)]
    pub fn open_from_held_private_root(
        root: &private_fs::HeldPrivateRoot,
        vault_id: Uuid,
    ) -> Result<Self> {
        root.revalidate()?;
        let store = Self::open_in_private_root(
            root.try_clone_directory()?,
            root.canonical_path(),
            vault_id,
            PrivateStoragePolicy::Standard,
        )?;
        root.revalidate()?;
        Ok(store)
    }

    /// Opens per-Vault sync state below a native-proven Android no-backup root.
    ///
    /// The caller must retain native `getNoBackupFilesDir()` provenance; the
    /// inspected capability itself exposes no constructor from an ambient path.
    /// Android owner/mode/link checks remain exact while an unsupported ACL
    /// query is accepted only in this dedicated native lane.
    ///
    /// # Errors
    /// Fails closed before or after open if root identity/privacy changes, or
    /// for invalid IDs, unsafe descendants, lease contention, and bad schema.
    #[cfg(target_os = "android")]
    #[doc(hidden)]
    pub fn open_from_android_no_backup_root(
        root: &private_fs::InspectedAndroidPrivateRoot,
        vault_id: Uuid,
    ) -> Result<Self> {
        root.revalidate()?;
        let store = Self::open_in_private_root(
            root.try_clone_directory()?,
            root.canonical_path(),
            vault_id,
            PrivateStoragePolicy::NativeAndroidNoBackup,
        )?;
        root.revalidate()?;
        Ok(store)
    }

    fn open_in_private_root(
        private_root: Dir,
        canonical_app_root: &Path,
        vault_id: Uuid,
        policy: PrivateStoragePolicy,
    ) -> Result<Self> {
        if vault_id.is_nil() {
            return Err(Error::InvalidVaultId);
        }
        let sync_root = create_or_open_storage_dir(&private_root, ROOT_DIRECTORY, policy)?;
        let version = create_or_open_storage_dir(&sync_root, VERSION_DIRECTORY, policy)?;
        let vaults = create_or_open_storage_dir(&version, VAULTS_DIRECTORY, policy)?;
        let vault_directory = create_or_open_storage_dir(&vaults, vault_id.to_string(), policy)?;
        let lease_file = acquire_sync_lease(&vault_directory, policy)?;
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
            harden_new_storage_file(&file, policy)?;
        }
        verify_storage_file(&file, policy)?;
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
        open_storage_file(&vault_directory, DATABASE_NAME, policy)?;

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

    /// Registers one immutable R3 intent with its initial durable state and event.
    ///
    /// `RemoteExistingBlocked` requires exact initial `NeedsReconcile` evidence and
    /// never enters an executable phase.
    ///
    /// # Errors
    /// Returns validation, immutable-identity, state-transition, or database errors.
    #[allow(clippy::too_many_lines)]
    pub fn register_mutation_intent(
        &mut self,
        intent: &MutationIntent,
        initial_evidence: Option<&MutationVerificationEvidence>,
    ) -> Result<MutationRegistrationOutcome> {
        validate_mutation_intent(intent)?;
        if let Some(evidence) = initial_evidence {
            validate_mutation_evidence(evidence)?;
            if evidence.operation_id != intent.operation_id {
                return Err(Error::MutationCollision);
            }
        }
        let blocked = intent.operation_kind == MutationOperationKind::RemoteExistingBlocked;
        if blocked {
            let evidence = initial_evidence.ok_or(Error::InvalidTransferEvidence)?;
            if evidence.disposition != MutationDisposition::NeedsReconcile
                || evidence.capture_phase != MutationEvidenceCapturePhase::Preflight
                || !evidence.forbidden_side_effect
                || evidence.outcome_code.is_none()
                || evidence.observed_account_id != intent.account_id
                || evidence.observed_remote_root_id != intent.remote_root_id
                || evidence.observed_remote_file_id != intent.remote_file_id
                || evidence.observed_parent_id != intent.source_parent_id
                || evidence.observed_path != intent.source_path
                || evidence.observed_local_revision != intent.expected_local_revision
                || evidence.observed_remote_revision != intent.expected_remote_revision
                || evidence.observed_operation_marker.as_deref()
                    != Some(intent.operation_marker.as_str())
            {
                return Err(Error::InvalidTransferEvidence);
            }
        } else if initial_evidence.is_some() {
            return Err(Error::InvalidStateTransition);
        }

        let transaction = self.connection.transaction()?;
        if let Some((fingerprint, marker)) =
            load_mutation_identity(&transaction, intent.operation_id)?
        {
            if fingerprint != intent.intent_fingerprint || marker != intent.operation_marker {
                return Err(Error::MutationCollision);
            }
            let state = load_mutation_state(&transaction, intent.operation_id)?
                .ok_or(Error::InvalidSchema)?;
            transaction.commit()?;
            return Ok(if state.phase == MutationPhase::Completed {
                MutationRegistrationOutcome::AlreadyCompleted
            } else {
                MutationRegistrationOutcome::AlreadyPresent
            });
        }
        if mutation_marker_exists(&transaction, &intent.operation_marker)? {
            return Err(Error::MutationCollision);
        }

        insert_mutation_intent(&transaction, intent)?;
        let initial_state = MutationState {
            operation_id: intent.operation_id,
            phase: MutationPhase::IntentDurable,
            attempt_number: 0,
            state_version: 0,
            disposition: None,
            next_attempt_at_unix_ms: None,
            retry_mode: None,
            resume_reference: None,
            last_evidence_id: None,
            outcome_code: None,
            updated_at_unix_ms: intent.registered_at_unix_ms,
        };
        insert_mutation_state(&transaction, &initial_state)?;
        insert_mutation_event(
            &transaction,
            intent.operation_id,
            0,
            0,
            MutationPhase::IntentDurable,
            None,
            None,
            None,
            intent.registered_at_unix_ms,
        )?;

        if let Some(evidence) = initial_evidence {
            insert_mutation_evidence(&transaction, evidence)?;
            let changed = transaction.execute(
                "UPDATE mutation_state
                 SET phase = 'needs_reconcile', state_version = 1,
                     disposition = 'needs_reconcile', last_evidence_id = ?1,
                     outcome_code = ?2, updated_at_unix_ms = ?3
                 WHERE operation_id = ?4 AND state_version = 0 AND phase = 'intent_durable'",
                params![
                    evidence.evidence_id.to_string(),
                    evidence.outcome_code,
                    u64_to_i64(evidence.captured_at_unix_ms)?,
                    intent.operation_id.to_string()
                ],
            )?;
            if changed != 1 {
                return Err(Error::MutationStateVersionMismatch);
            }
            insert_mutation_event(
                &transaction,
                intent.operation_id,
                0,
                1,
                MutationPhase::NeedsReconcile,
                Some(MutationDisposition::NeedsReconcile),
                Some(evidence.evidence_id),
                evidence.outcome_code.as_deref(),
                evidence.captured_at_unix_ms,
            )?;
        }
        transaction.commit()?;
        Ok(MutationRegistrationOutcome::Registered)
    }

    /// Reads one current R3 mutation state without exposing private capabilities.
    ///
    /// # Errors
    /// Returns an invalid-operation or database error.
    pub fn mutation_state(&self, operation_id: Uuid) -> Result<Option<MutationState>> {
        if operation_id.is_nil() {
            return Err(Error::MutationNotFound);
        }
        load_mutation_state(&self.connection, operation_id)
    }

    /// Reads append-only state-transition history for one mutation.
    ///
    /// # Errors
    /// Returns an invalid-operation, malformed-record, or database error.
    pub fn mutation_events(&self, operation_id: Uuid) -> Result<Vec<MutationEvent>> {
        if operation_id.is_nil() {
            return Err(Error::MutationNotFound);
        }
        load_mutation_events(&self.connection, operation_id)
    }

    /// Claims a durable intent or due retry using the caller's expected state version.
    ///
    /// # Errors
    /// Returns an unknown-operation, stale-version, invalid-transition, or database error.
    pub fn claim_mutation(
        &mut self,
        operation_id: Uuid,
        expected_state_version: u64,
        now_unix_ms: u64,
    ) -> Result<MutationState> {
        u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let (_, operation_kind) =
            load_mutation_kind(&transaction, operation_id)?.ok_or(Error::MutationNotFound)?;
        if operation_kind == MutationOperationKind::RemoteExistingBlocked {
            return Err(Error::MutationNeedsReconcile);
        }
        let state =
            load_mutation_state(&transaction, operation_id)?.ok_or(Error::MutationNotFound)?;
        if state.state_version != expected_state_version {
            return Err(Error::MutationStateVersionMismatch);
        }
        let retry = state.phase == MutationPhase::RetryScheduled;
        if !matches!(
            state.phase,
            MutationPhase::IntentDurable | MutationPhase::RetryScheduled
        ) || state
            .next_attempt_at_unix_ms
            .is_some_and(|due| due > now_unix_ms)
        {
            return Err(Error::InvalidStateTransition);
        }
        let next = MutationState {
            operation_id,
            phase: MutationPhase::Running,
            attempt_number: state.attempt_number + u32::from(retry),
            state_version: state.state_version + 1,
            disposition: None,
            next_attempt_at_unix_ms: None,
            retry_mode: None,
            resume_reference: None,
            last_evidence_id: state.last_evidence_id,
            outcome_code: None,
            updated_at_unix_ms: now_unix_ms,
        };
        update_mutation_state(&transaction, &next, state.state_version)?;
        insert_mutation_event(
            &transaction,
            operation_id,
            next.attempt_number,
            next.state_version,
            next.phase,
            None,
            None,
            None,
            now_unix_ms,
        )?;
        transaction.commit()?;
        Ok(next)
    }

    /// Persists exact outcome evidence, the resulting state, and one event atomically.
    ///
    /// # Errors
    /// Returns invalid evidence, stale-version, invalid-transition, or database errors.
    pub fn record_mutation_outcome(
        &mut self,
        operation_id: Uuid,
        expected_state_version: u64,
        evidence: &MutationVerificationEvidence,
        transition: &MutationOutcomeTransition,
    ) -> Result<MutationState> {
        validate_mutation_evidence(evidence)?;
        if evidence.operation_id != operation_id {
            return Err(Error::MutationCollision);
        }
        let transaction = self.connection.transaction()?;
        let state =
            load_mutation_state(&transaction, operation_id)?.ok_or(Error::MutationNotFound)?;
        if state.state_version != expected_state_version {
            return Err(Error::MutationStateVersionMismatch);
        }
        if evidence.attempt_number != state.attempt_number
            || !matches!(
                state.phase,
                MutationPhase::Running | MutationPhase::NeedsReconcile
            )
        {
            return Err(Error::InvalidStateTransition);
        }
        if matches!(transition, MutationOutcomeTransition::VerifiedApplied) {
            validate_verified_applied_evidence(&transaction, evidence)?;
        }
        let (phase, disposition, next_attempt, retry_mode, resume_reference) =
            transition_target(state.phase, evidence, transition)?;
        insert_mutation_evidence(&transaction, evidence)?;
        let next = MutationState {
            operation_id,
            phase,
            attempt_number: state.attempt_number,
            state_version: state.state_version + 1,
            disposition: Some(disposition),
            next_attempt_at_unix_ms: next_attempt,
            retry_mode,
            resume_reference,
            last_evidence_id: Some(evidence.evidence_id),
            outcome_code: evidence.outcome_code.clone(),
            updated_at_unix_ms: evidence.captured_at_unix_ms,
        };
        update_mutation_state(&transaction, &next, state.state_version)?;
        insert_mutation_event(
            &transaction,
            operation_id,
            next.attempt_number,
            next.state_version,
            next.phase,
            next.disposition,
            Some(evidence.evidence_id),
            evidence.outcome_code.as_deref(),
            evidence.captured_at_unix_ms,
        )?;
        transaction.commit()?;
        Ok(next)
    }

    /// Persists one immutable R3.1 conflict-evidence envelope without classifying content.
    ///
    /// Exact reruns return `AlreadyPresent`; any differing durable identity fails closed.
    ///
    /// # Errors
    /// Returns validation, ownership, immutable-identity, or database errors.
    pub fn record_conflict_evidence(
        &mut self,
        evidence: &ConflictEvidence,
    ) -> Result<ConflictEvidenceRegistrationOutcome> {
        validate_conflict_evidence(evidence)?;
        let transaction = self.connection.transaction()?;
        if load_mutation_kind(&transaction, evidence.operation_id)?.is_none() {
            return Err(Error::MutationNotFound);
        }
        if let Some(existing) =
            load_conflict_evidence_fingerprint(&transaction, &evidence.conflict_id)?
        {
            if existing == evidence.evidence_fingerprint {
                transaction.commit()?;
                return Ok(ConflictEvidenceRegistrationOutcome::AlreadyPresent);
            }
            return Err(Error::MutationCollision);
        }
        if let Some(operation_id) = evidence.conflict_copy_operation_id {
            let (_, kind) =
                load_mutation_kind(&transaction, operation_id)?.ok_or(Error::MutationNotFound)?;
            if kind != MutationOperationKind::ConflictCopyPublish {
                return Err(Error::MutationCollision);
            }
        }
        for evidence_id in [
            evidence.base_evidence_id,
            evidence.local_evidence_id,
            evidence.remote_evidence_id,
        ]
        .into_iter()
        .flatten()
        {
            if !evidence_belongs_to_operation(&transaction, evidence_id, evidence.operation_id)? {
                return Err(Error::MutationCollision);
            }
        }
        insert_conflict_evidence(&transaction, evidence)?;
        transaction.commit()?;
        Ok(ConflictEvidenceRegistrationOutcome::Registered)
    }

    /// Reads one immutable conflict-evidence envelope by its stable conflict identity.
    ///
    /// # Errors
    /// Returns invalid identity, malformed durable rows, or database errors.
    pub fn conflict_evidence(&self, conflict_id: &str) -> Result<Option<ConflictEvidence>> {
        validate_redacted_code(conflict_id)?;
        load_conflict_evidence(&self.connection, conflict_id)
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

    /// Reads one exact durable remote entry by provider file ID.
    ///
    /// # Errors
    /// Rejects malformed identifiers or persisted metadata evidence.
    pub fn remote_entry(&self, file_id: &str) -> Result<Option<RemoteEntry>> {
        validate_remote_id(file_id)?;
        let persisted: Option<PersistedRemoteEntry> = self
            .connection
            .query_row(
                "SELECT file_id, parent_id, portable_path, kind,
                        content_hash_algorithm, content_hash, remote_revision
                 FROM remote_entries WHERE file_id = ?1",
                [file_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((file_id, parent_id, path, kind, hash_algorithm, hash, remote_revision)) =
            persisted
        else {
            return Ok(None);
        };
        let kind = match kind.as_str() {
            "file" => RemoteEntryKind::File,
            "folder" => RemoteEntryKind::Folder,
            _ => return Err(Error::InvalidSchema),
        };
        let content_hash = match (hash_algorithm.as_deref(), hash) {
            (None, None) => None,
            (Some(algorithm), Some(hash)) => Some(RemoteContentHash::new(
                match algorithm {
                    "md5" => RemoteHashAlgorithm::Md5,
                    "sha1" => RemoteHashAlgorithm::Sha1,
                    "sha256" => RemoteHashAlgorithm::Sha256,
                    _ => return Err(Error::InvalidSchema),
                },
                hash,
            )?),
            _ => return Err(Error::InvalidSchema),
        };
        let entry = RemoteEntry {
            file_id,
            parent_id,
            path,
            kind,
            content_hash,
            remote_revision,
        };
        entry.validate()?;
        Ok(Some(entry))
    }

    /// Reads verified base evidence for one exact remote file.
    ///
    /// # Errors
    /// Rejects malformed IDs or partially persisted base evidence.
    pub fn remote_base(&self, file_id: &str) -> Result<Option<RemoteBaseEvidence>> {
        validate_remote_id(file_id)?;
        let persisted: Option<PersistedRemoteBase> = self
            .connection
            .query_row(
                "SELECT base_local_revision, base_remote_revision, base_content_hash, base_byte_length
                 FROM remote_entries WHERE file_id = ?1",
                [file_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        match persisted {
            None | Some((None, None, None, None)) => Ok(None),
            Some((
                Some(local_revision),
                Some(remote_revision),
                Some(content_hash),
                Some(byte_length),
            )) => {
                validate_revision(&local_revision)?;
                validate_remote_token(&remote_revision)?;
                RemoteContentHash::new(RemoteHashAlgorithm::Sha256, content_hash.clone())?;
                let byte_length = u64::try_from(byte_length).map_err(|_| Error::InvalidSchema)?;
                Ok(Some(RemoteBaseEvidence {
                    local_revision,
                    remote_revision,
                    content_hash,
                    byte_length,
                }))
            }
            Some(_) => Err(Error::InvalidSchema),
        }
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

    /// Registers complete durable transfer evidence before any side effect.
    ///
    /// # Errors
    /// Rejects unbound state, malformed evidence, marker reuse, or conflicting operation IDs.
    pub fn register_transfer(
        &mut self,
        transfer: &TransferRecord,
    ) -> Result<TransferRegistrationOutcome> {
        validate_new_transfer(transfer)?;
        let transaction = self.connection.transaction()?;
        require_state(&transaction, self.vault_id)?;
        if let Some(existing) = load_transfer(&transaction, transfer.operation_id)? {
            if !existing.same_registration(transfer) {
                return Err(Error::TransferCollision);
            }
            let outcome = if existing.phase == TransferPhase::Completed {
                TransferRegistrationOutcome::AlreadyCompleted
            } else {
                TransferRegistrationOutcome::AlreadyPresent
            };
            transaction.commit()?;
            return Ok(outcome);
        }
        let marker_owner: Option<String> = transaction
            .query_row(
                "SELECT operation_id FROM transfers WHERE operation_marker = ?1",
                [&transfer.operation_marker],
                |row| row.get(0),
            )
            .optional()?;
        if marker_owner.is_some() {
            return Err(Error::TransferCollision);
        }
        insert_transfer(&transaction, transfer)?;
        transaction.commit()?;
        Ok(TransferRegistrationOutcome::Registered)
    }

    /// Claims the oldest due transfer in one transaction.
    ///
    /// Reconciliation and authentication pauses are never claimed implicitly.
    ///
    /// # Errors
    /// Returns invalid persisted evidence, timestamp, or database errors.
    pub fn claim_next_transfer(&mut self, now_unix_ms: u64) -> Result<Option<TransferRecord>> {
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let candidate = {
            let mut statement = transaction.prepare(
                "SELECT operation_id, direction, portable_path, remote_parent_id,
                        remote_file_id, display_name, expected_local_revision,
                        expected_remote_revision, sha256, byte_length, mime_class,
                        operation_marker, stage_reference, base_reference, phase,
                        attempt_count, next_attempt_at_unix_ms, created_at_unix_ms,
                        updated_at_unix_ms, last_error_code, verified_local_revision,
                        verified_remote_revision
                 FROM transfers
                 WHERE phase IN ('pending', 'retry_scheduled')
                   AND next_attempt_at_unix_ms <= ?1
                 ORDER BY created_at_unix_ms, operation_id
                 LIMIT 1",
            )?;
            statement.query_row([now], row_to_transfer).optional()?
        };
        let Some(mut transfer) = candidate.transpose()? else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE transfers SET phase = ?1, updated_at_unix_ms = ?2
             WHERE operation_id = ?3 AND phase IN ('pending', 'retry_scheduled')",
            params![
                TransferPhase::Running.as_str(),
                now,
                transfer.operation_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        transfer.phase = TransferPhase::Running;
        transfer.updated_at_unix_ms = now_unix_ms;
        transaction.commit()?;
        Ok(Some(transfer))
    }

    /// Schedules a retry only after the caller has established that replay is safe.
    ///
    /// # Errors
    /// Rejects missing transfers, invalid codes/timestamps, or invalid transitions.
    pub fn schedule_transfer_retry(
        &mut self,
        operation_id: Uuid,
        next_attempt_at_unix_ms: u64,
        error_code: &str,
        updated_at_unix_ms: u64,
    ) -> Result<()> {
        if operation_id.is_nil() {
            return Err(Error::TransferNotFound);
        }
        validate_redacted_code(error_code)?;
        let next = u64_to_i64(next_attempt_at_unix_ms)?;
        let updated = u64_to_i64(updated_at_unix_ms)?;
        let existing =
            load_transfer(&self.connection, operation_id)?.ok_or(Error::TransferNotFound)?;
        if existing.phase == TransferPhase::RetryScheduled
            && existing.next_attempt_at_unix_ms == next_attempt_at_unix_ms
            && existing.last_error_code.as_deref() == Some(error_code)
        {
            return Ok(());
        }
        if updated_at_unix_ms < existing.updated_at_unix_ms {
            return Err(Error::InvalidStateTransition);
        }
        let changed = self.connection.execute(
            "UPDATE transfers
             SET phase = ?1, attempt_count = attempt_count + 1,
                 next_attempt_at_unix_ms = ?2, updated_at_unix_ms = ?3,
                 last_error_code = ?4
             WHERE operation_id = ?5
               AND phase IN ('running', 'auth_required', 'needs_reconcile')",
            params![
                TransferPhase::RetryScheduled.as_str(),
                next,
                updated,
                error_code,
                operation_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        Ok(())
    }

    /// Pauses a running transfer while offline without consuming a retry attempt.
    ///
    /// # Errors
    /// Rejects missing transfers, invalid codes/timestamps, or invalid transitions.
    pub fn pause_transfer_offline(
        &mut self,
        operation_id: Uuid,
        next_attempt_at_unix_ms: u64,
        error_code: &str,
        updated_at_unix_ms: u64,
    ) -> Result<()> {
        if operation_id.is_nil() {
            return Err(Error::TransferNotFound);
        }
        validate_redacted_code(error_code)?;
        let next = u64_to_i64(next_attempt_at_unix_ms)?;
        let updated = u64_to_i64(updated_at_unix_ms)?;
        let existing =
            load_transfer(&self.connection, operation_id)?.ok_or(Error::TransferNotFound)?;
        if existing.phase == TransferPhase::RetryScheduled
            && existing.next_attempt_at_unix_ms == next_attempt_at_unix_ms
            && existing.last_error_code.as_deref() == Some(error_code)
        {
            return Ok(());
        }
        if updated_at_unix_ms < existing.updated_at_unix_ms {
            return Err(Error::InvalidStateTransition);
        }
        let changed = self.connection.execute(
            "UPDATE transfers
             SET phase = ?1, next_attempt_at_unix_ms = ?2,
                 updated_at_unix_ms = ?3, last_error_code = ?4
             WHERE operation_id = ?5 AND phase = 'running'",
            params![
                TransferPhase::RetryScheduled.as_str(),
                next,
                updated,
                error_code,
                operation_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        Ok(())
    }

    /// Pauses a running transfer without persisting provider errors or credentials.
    ///
    /// # Errors
    /// Rejects missing transfers, malformed redacted codes, or invalid transitions.
    pub fn mark_transfer_auth_required(
        &mut self,
        operation_id: Uuid,
        error_code: &str,
        updated_at_unix_ms: u64,
    ) -> Result<()> {
        self.mark_transfer_stopped(
            operation_id,
            TransferPhase::AuthRequired,
            error_code,
            updated_at_unix_ms,
        )
    }

    /// Reschedules every authorization-paused transfer after the caller has
    /// obtained a fresh credential for the exact bound account.
    ///
    /// # Errors
    /// Rejects invalid timestamps or unavailable durable storage.
    pub fn resume_auth_required_transfers(&mut self, now_unix_ms: u64) -> Result<u64> {
        let now = u64_to_i64(now_unix_ms)?;
        let changed = self.connection.execute(
            "UPDATE transfers
             SET phase = ?1, attempt_count = attempt_count + 1,
                 next_attempt_at_unix_ms = ?2, updated_at_unix_ms = ?2,
                 last_error_code = ?3
             WHERE phase = 'auth_required' AND updated_at_unix_ms <= ?2",
            params![TransferPhase::RetryScheduled.as_str(), now, "auth_restored"],
        )?;
        u64::try_from(changed).map_err(|_| Error::InvalidSchema)
    }

    /// Stops a transfer whose side-effect outcome or revision is ambiguous.
    ///
    /// # Errors
    /// Rejects missing transfers, malformed redacted codes, or invalid transitions.
    pub fn mark_transfer_needs_reconcile(
        &mut self,
        operation_id: Uuid,
        error_code: &str,
        updated_at_unix_ms: u64,
    ) -> Result<()> {
        self.mark_transfer_stopped(
            operation_id,
            TransferPhase::NeedsReconcile,
            error_code,
            updated_at_unix_ms,
        )
    }

    /// Releases exactly one stopped transfer for an explicit reconciliation run.
    ///
    /// This transition does not claim that an earlier side effect succeeded or was
    /// absent. It preserves every expected identity and opaque stage/base reference,
    /// and only changes `NeedsReconcile` to a due `RetryScheduled` row carrying the
    /// redacted `reconcile_requested` signal. After claiming it, the executor must
    /// inspect the exact durable local and remote identities before it may complete
    /// the transfer or perform any replay proven safe by that inspection.
    ///
    /// # Errors
    /// Rejects missing transfers, stale timestamps, or any phase other than
    /// `NeedsReconcile`. A second request is therefore not a blind replay.
    pub fn requeue_transfer_for_reconciliation(
        &mut self,
        operation_id: Uuid,
        now_unix_ms: u64,
    ) -> Result<()> {
        if operation_id.is_nil() {
            return Err(Error::TransferNotFound);
        }
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let existing = load_transfer(&transaction, operation_id)?.ok_or(Error::TransferNotFound)?;
        if existing.phase != TransferPhase::NeedsReconcile
            || now_unix_ms < existing.updated_at_unix_ms
        {
            return Err(Error::InvalidStateTransition);
        }
        let changed = transaction.execute(
            "UPDATE transfers
             SET phase = ?1, attempt_count = attempt_count + 1,
                 next_attempt_at_unix_ms = ?2, updated_at_unix_ms = ?2,
                 last_error_code = ?3
             WHERE operation_id = ?4 AND phase = 'needs_reconcile'
               AND updated_at_unix_ms <= ?2",
            params![
                TransferPhase::RetryScheduled.as_str(),
                now,
                "reconcile_requested",
                operation_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Publishes an opaque private base-object reference without exposing an ambient path.
    ///
    /// The operation is exact-idempotent and may precede the final completion transaction.
    ///
    /// # Errors
    /// Rejects mismatched references, missing transfers, or invalid transitions.
    pub fn publish_transfer_base_reference(
        &mut self,
        operation_id: Uuid,
        base_reference: &str,
        updated_at_unix_ms: u64,
    ) -> Result<()> {
        validate_private_reference(base_reference)?;
        let updated = u64_to_i64(updated_at_unix_ms)?;
        let existing =
            load_transfer(&self.connection, operation_id)?.ok_or(Error::TransferNotFound)?;
        if existing.base_reference.as_deref() == Some(base_reference) {
            return Ok(());
        }
        if existing.base_reference.is_some() || updated_at_unix_ms < existing.updated_at_unix_ms {
            return Err(Error::TransferCollision);
        }
        let changed = self.connection.execute(
            "UPDATE transfers SET base_reference = ?1, updated_at_unix_ms = ?2
             WHERE operation_id = ?3
               AND phase IN ('running', 'needs_reconcile')
               AND base_reference IS NULL",
            params![base_reference, updated, operation_id.to_string()],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        Ok(())
    }

    /// Commits verified exact identities, a base reference, a completed tombstone, and
    /// redacted history atomically.
    ///
    /// # Errors
    /// Rejects stale expected identities, conflicting completion, or invalid transitions.
    pub fn complete_verified_transfer(
        &mut self,
        operation_id: Uuid,
        completion: &TransferCompletion,
    ) -> Result<TransferCompletionOutcome> {
        if operation_id.is_nil() {
            return Err(Error::TransferNotFound);
        }
        completion.validate()?;
        let transaction = self.connection.transaction()?;
        let existing = load_transfer(&transaction, operation_id)?.ok_or(Error::TransferNotFound)?;
        let mutation_id = transfer_mutation_id(operation_id);
        let mutation_state = active_transfer_mutation_state(&transaction, &mutation_id)?;
        if existing.phase == TransferPhase::Completed {
            let history: Option<(String, i64)> = transaction
                .query_row(
                    "SELECT outcome_code, occurred_at_unix_ms FROM transfer_history
                     WHERE operation_id = ?1",
                    [operation_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let same = existing.remote_file_id.as_deref() == Some(&completion.remote_file_id)
                && existing.verified_remote_revision.as_deref()
                    == Some(&completion.remote_revision)
                && existing.verified_local_revision.as_deref() == Some(&completion.local_revision)
                && existing.base_reference.as_deref() == Some(&completion.base_reference)
                && history
                    == Some((
                        completion.outcome_code.clone(),
                        u64_to_i64(completion.occurred_at_unix_ms)?,
                    ));
            let mutation_consistent =
                mutation_state.is_none_or(|state| state == LocalMutationState::Committed);
            if same && mutation_consistent {
                transaction.commit()?;
                return Ok(TransferCompletionOutcome::AlreadyCompleted);
            }
            return Err(Error::TransferCollision);
        }
        if !matches!(
            existing.phase,
            TransferPhase::Running | TransferPhase::NeedsReconcile
        ) || existing.updated_at_unix_ms > completion.occurred_at_unix_ms
            || existing
                .remote_file_id
                .as_ref()
                .is_some_and(|value| value != &completion.remote_file_id)
            || existing
                .expected_remote_revision
                .as_ref()
                .is_some_and(|value| value != &completion.remote_revision)
            || (existing.direction == TransferDirection::Upload
                && existing
                    .expected_local_revision
                    .as_ref()
                    .is_some_and(|value| value != &completion.local_revision))
            || existing
                .base_reference
                .as_ref()
                .is_some_and(|value| value != &completion.base_reference)
        {
            return Err(Error::InvalidStateTransition);
        }
        if mutation_state.is_some() && existing.direction != TransferDirection::Download {
            return Err(Error::TransferChangeMismatch);
        }
        if mutation_state.is_some_and(|state| state != LocalMutationState::Applying) {
            return Err(Error::InvalidStateTransition);
        }
        let occurred = u64_to_i64(completion.occurred_at_unix_ms)?;
        transaction.execute(
            "INSERT INTO transfer_history(operation_id, outcome_code, occurred_at_unix_ms)
             VALUES (?1, ?2, ?3)",
            params![operation_id.to_string(), completion.outcome_code, occurred],
        )?;
        let changed = transaction.execute(
            "UPDATE transfers
             SET remote_file_id = ?1, base_reference = ?2, phase = ?3,
                 next_attempt_at_unix_ms = ?4, updated_at_unix_ms = ?4,
                 last_error_code = NULL, verified_local_revision = ?5,
                 verified_remote_revision = ?6
             WHERE operation_id = ?7 AND phase IN ('running', 'needs_reconcile')",
            params![
                completion.remote_file_id,
                completion.base_reference,
                TransferPhase::Completed.as_str(),
                occurred,
                completion.local_revision,
                completion.remote_revision,
                operation_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        update_remote_base_if_present(&transaction, &existing, completion)?;
        if mutation_state == Some(LocalMutationState::Applying) {
            commit_transfer_mutation(&transaction, &mutation_id)?;
        }
        transaction.commit()?;
        Ok(TransferCompletionOutcome::Completed)
    }

    /// Reads one durable transfer by operation ID.
    ///
    /// # Errors
    /// Returns invalid persisted evidence or database errors.
    pub fn transfer(&self, operation_id: Uuid) -> Result<Option<TransferRecord>> {
        load_transfer(&self.connection, operation_id)
    }

    /// Returns active, non-completed transfer count.
    ///
    /// # Errors
    /// Returns invalid count or database errors.
    pub fn transfer_count(&self) -> Result<u64> {
        query_count(
            &self.connection,
            "SELECT COUNT(*) FROM transfers WHERE phase != 'completed'",
        )
    }

    /// Returns redacted counts for every durable transfer phase.
    ///
    /// # Errors
    /// Returns a database or invalid count error.
    pub fn transfer_summary(&self) -> Result<TransferSummary> {
        let count = |phase: &str| -> Result<u64> {
            let value: i64 = self.connection.query_row(
                "SELECT COUNT(*) FROM transfers WHERE phase = ?1",
                [phase],
                |row| row.get(0),
            )?;
            value.try_into().map_err(|_| Error::InvalidSchema)
        };
        Ok(TransferSummary {
            pending: count(TransferPhase::Pending.as_str())?,
            running: count(TransferPhase::Running.as_str())?,
            retry_scheduled: count(TransferPhase::RetryScheduled.as_str())?,
            auth_required: count(TransferPhase::AuthRequired.as_str())?,
            needs_reconcile: count(TransferPhase::NeedsReconcile.as_str())?,
            completed: count(TransferPhase::Completed.as_str())?,
        })
    }

    fn mark_transfer_stopped(
        &mut self,
        operation_id: Uuid,
        target: TransferPhase,
        error_code: &str,
        updated_at_unix_ms: u64,
    ) -> Result<()> {
        if operation_id.is_nil() {
            return Err(Error::TransferNotFound);
        }
        if !matches!(
            target,
            TransferPhase::AuthRequired | TransferPhase::NeedsReconcile
        ) {
            return Err(Error::InvalidStateTransition);
        }
        validate_redacted_code(error_code)?;
        let updated = u64_to_i64(updated_at_unix_ms)?;
        let existing =
            load_transfer(&self.connection, operation_id)?.ok_or(Error::TransferNotFound)?;
        if existing.phase == target && existing.last_error_code.as_deref() == Some(error_code) {
            return Ok(());
        }
        if updated_at_unix_ms < existing.updated_at_unix_ms {
            return Err(Error::InvalidStateTransition);
        }
        let changed = self.connection.execute(
            "UPDATE transfers SET phase = ?1, updated_at_unix_ms = ?2, last_error_code = ?3
             WHERE operation_id = ?4 AND phase = 'running'",
            params![
                target.as_str(),
                updated,
                error_code,
                operation_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(Error::InvalidStateTransition);
        }
        Ok(())
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
                "INSERT INTO change_batch_mutations(
                    batch_id, mutation_id, dependency_kind, operation_id,
                    committed_evidence_id, state
                 ) VALUES (?1, ?2, 'legacy_v3', NULL, NULL, ?3)",
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

    /// Starts a transfer-coupled incremental page in one transaction.
    ///
    /// The resolved metadata, exact download registrations, and one declared local
    /// mutation per download become durable together. Known removals, moves, kind
    /// changes, and file revisions without a corresponding download fail closed.
    ///
    /// # Errors
    /// Rejects stale cursors, active batches, unsupported changes, mismatched
    /// downloads, duplicate identities, or malformed durable evidence.
    pub fn begin_transfer_change_batch(
        &mut self,
        batch_id: Uuid,
        expected_cursor: &str,
        next_cursor: &str,
        changes: &[RemoteChange],
        downloads: &[TransferRecord],
    ) -> Result<()> {
        if batch_id.is_nil() {
            return Err(Error::InvalidStateTransition);
        }
        validate_remote_token(expected_cursor)?;
        validate_remote_token(next_cursor)?;
        if changes.len() > crate::MAX_SCAN_PAGE_ENTRIES {
            return Err(Error::InvalidRemoteEntry);
        }
        for change in changes {
            change.validate()?;
        }
        for transfer in downloads {
            validate_new_transfer(transfer)?;
            if transfer.direction != TransferDirection::Download
                || transfer.expected_remote_revision.is_none()
            {
                return Err(Error::TransferChangeMismatch);
            }
        }

        let transaction = self.connection.transaction()?;
        let state = require_state(&transaction, self.vault_id)?;
        if state.phase != SyncPhase::Ready
            || state.durable_cursor.as_deref() != Some(expected_cursor)
        {
            return Err(Error::CursorMismatch);
        }
        if load_change_batch(&transaction)?.is_some() {
            return Err(Error::BatchAlreadyActive);
        }

        validate_resolved_transfer_changes(&transaction, changes, downloads)?;

        transaction.execute(
            "INSERT INTO change_batch(singleton, batch_id, expected_cursor, next_cursor)
             VALUES (1, ?1, ?2, ?3)",
            params![batch_id.to_string(), expected_cursor, next_cursor],
        )?;
        for change in changes {
            if let RemoteChange::Upsert(entry) = change {
                upsert_remote_entry(&transaction, entry)?;
            }
        }
        for transfer in downloads {
            register_transfer_in_transaction(&transaction, transfer)?;
            transaction.execute(
                "INSERT INTO change_batch_mutations(
                    batch_id, mutation_id, dependency_kind, operation_id,
                    committed_evidence_id, state
                 ) VALUES (?1, ?2, 'legacy_v3', NULL, NULL, ?3)",
                params![
                    batch_id.to_string(),
                    transfer_mutation_id(transfer.operation_id),
                    LocalMutationState::Pending.as_str()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Starts one typed R3 cursor batch without granting any provider capability.
    ///
    /// Every dependency is bound to an existing immutable intent whose operation
    /// kind exactly matches the declared cursor-dependency kind. Legacy APIs
    /// cannot complete this batch.
    ///
    /// # Errors
    /// Returns cursor, duplicate-identity, missing-intent, or database errors.
    pub fn begin_r3_change_batch(
        &mut self,
        batch_id: Uuid,
        expected_cursor: &str,
        next_cursor: &str,
        dependencies: &[ChangeBatchDependency],
    ) -> Result<()> {
        if batch_id.is_nil() {
            return Err(Error::InvalidStateTransition);
        }
        validate_remote_token(expected_cursor)?;
        validate_remote_token(next_cursor)?;
        let declared = dependencies
            .iter()
            .map(|dependency| dependency.operation_id)
            .collect::<BTreeSet<_>>();
        if declared.len() != dependencies.len() || declared.contains(&Uuid::nil()) {
            return Err(Error::MutationCollision);
        }

        let transaction = self.connection.transaction()?;
        let state = require_state(&transaction, self.vault_id)?;
        if state.phase != SyncPhase::Ready
            || state.durable_cursor.as_deref() != Some(expected_cursor)
        {
            return Err(Error::CursorMismatch);
        }
        if load_change_batch(&transaction)?.is_some() {
            return Err(Error::BatchAlreadyActive);
        }
        for dependency in dependencies {
            let (_, operation_kind) = load_mutation_kind(&transaction, dependency.operation_id)?
                .ok_or(Error::MutationNotFound)?;
            if operation_kind != dependency.kind.required_operation_kind()
                || load_mutation_state(&transaction, dependency.operation_id)?.is_none()
            {
                return Err(Error::MutationCollision);
            }
        }
        transaction.execute(
            "INSERT INTO change_batch(singleton, batch_id, expected_cursor, next_cursor)
             VALUES (1, ?1, ?2, ?3)",
            params![batch_id.to_string(), expected_cursor, next_cursor],
        )?;
        for dependency in dependencies {
            transaction.execute(
                "INSERT INTO change_batch_mutations(
                    batch_id, mutation_id, dependency_kind, operation_id,
                    committed_evidence_id, state
                 ) VALUES (?1, ?2, ?3, ?4, NULL, 'pending')",
                params![
                    batch_id.to_string(),
                    dependency.operation_id.to_string(),
                    dependency.kind.as_str(),
                    dependency.operation_id.to_string(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Binds one completed R3 operation to the exact evidence required by its batch.
    ///
    /// The operation must have completed with exact post-verification evidence and
    /// a matching immutable completion event. Repeating the same bind is idempotent.
    ///
    /// # Errors
    /// Returns missing-batch, identity, incomplete-evidence, reconciliation, or database errors.
    pub fn commit_r3_change_dependency(
        &mut self,
        batch_id: Uuid,
        dependency: ChangeBatchDependency,
        evidence_id: Uuid,
    ) -> Result<()> {
        if batch_id.is_nil() || dependency.operation_id.is_nil() || evidence_id.is_nil() {
            return Err(Error::InvalidStateTransition);
        }
        let transaction = self.connection.transaction()?;
        let batch = load_change_batch(&transaction)?.ok_or(Error::NoActiveBatch)?;
        if batch.batch_id != batch_id {
            return Err(Error::NoActiveBatch);
        }
        let row = transaction
            .query_row(
                "SELECT dependency_kind, operation_id, committed_evidence_id, state
                 FROM change_batch_mutations
                 WHERE batch_id = ?1 AND mutation_id = ?2",
                params![batch_id.to_string(), dependency.operation_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or(Error::UnknownMutation)?;
        if row.0 != dependency.kind.as_str()
            || row.1.as_deref() != Some(dependency.operation_id.to_string().as_str())
        {
            return Err(Error::MutationCollision);
        }
        let (_, operation_kind) = load_mutation_kind(&transaction, dependency.operation_id)?
            .ok_or(Error::MutationNotFound)?;
        if operation_kind != dependency.kind.required_operation_kind() {
            return Err(Error::MutationCollision);
        }
        match LocalMutationState::parse(&row.3)? {
            LocalMutationState::Committed => {
                if row.2.as_deref() != Some(evidence_id.to_string().as_str()) {
                    return Err(Error::MutationCollision);
                }
            }
            LocalMutationState::Pending => {}
            LocalMutationState::Applying | LocalMutationState::NeedsReconcile => {
                return Err(Error::MutationNeedsReconcile);
            }
        }
        require_exact_r3_completion_evidence(&transaction, dependency.operation_id, evidence_id)?;
        if LocalMutationState::parse(&row.3)? == LocalMutationState::Committed {
            transaction.commit()?;
            return Ok(());
        }
        let changed = transaction.execute(
            "UPDATE change_batch_mutations
             SET state = 'committed', committed_evidence_id = ?1
             WHERE batch_id = ?2 AND mutation_id = ?3 AND dependency_kind = ?4
               AND operation_id = ?5 AND state = 'pending' AND committed_evidence_id IS NULL",
            params![
                evidence_id.to_string(),
                batch_id.to_string(),
                dependency.operation_id.to_string(),
                dependency.kind.as_str(),
                dependency.operation_id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(Error::MutationStateVersionMismatch);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Advances the cursor only after every typed R3 dependency has exact evidence.
    ///
    /// # Errors
    /// Returns missing-batch, incomplete-dependency, changed-cursor, or database errors.
    pub fn commit_r3_change_batch(&mut self, batch_id: Uuid, now_unix_ms: u64) -> Result<()> {
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let batch = load_change_batch(&transaction)?.ok_or(Error::NoActiveBatch)?;
        if batch.batch_id != batch_id {
            return Err(Error::NoActiveBatch);
        }
        if legacy_v3_dependency_count(&transaction, batch_id)? != 0
            || transfer_backed_mutation_count(&transaction, batch_id)? != 0
        {
            return Err(Error::InvalidStateTransition);
        }
        let incomplete: i64 = transaction.query_row(
            "SELECT COUNT(*)
             FROM change_batch_mutations AS dependency
             LEFT JOIN mutation_intents AS intent
               ON intent.operation_id = dependency.operation_id
             LEFT JOIN mutation_state AS state
               ON state.operation_id = dependency.operation_id
             LEFT JOIN mutation_verification_evidence AS evidence
               ON evidence.evidence_id = dependency.committed_evidence_id
             WHERE dependency.batch_id = ?1 AND (
               dependency.dependency_kind NOT IN (
                 'mutation', 'merge_publication', 'conflict_copy_publication', 'base_publication'
               )
               OR dependency.operation_id IS NULL
               OR dependency.committed_evidence_id IS NULL
               OR dependency.state != 'committed'
               OR intent.operation_id IS NULL
               OR (dependency.dependency_kind = 'mutation' AND intent.operation_kind != 'local_publish')
               OR (dependency.dependency_kind = 'merge_publication' AND intent.operation_kind != 'merge_publish')
               OR (dependency.dependency_kind = 'conflict_copy_publication' AND intent.operation_kind != 'conflict_copy_publish')
               OR (dependency.dependency_kind = 'base_publication' AND intent.operation_kind != 'base_publish')
               OR state.phase != 'completed'
               OR state.disposition != 'verified_applied'
               OR state.last_evidence_id != dependency.committed_evidence_id
               OR evidence.operation_id != dependency.operation_id
               OR evidence.capture_phase != 'post_verify'
               OR evidence.disposition != 'verified_applied'
               OR evidence.forbidden_side_effect != 0
               OR NOT EXISTS (
                 SELECT 1 FROM mutation_events AS event
               WHERE event.operation_id = dependency.operation_id
                   AND event.evidence_id = dependency.committed_evidence_id
                   AND event.attempt_number = state.attempt_number
                   AND event.state_version = state.state_version
                   AND event.phase = 'completed'
                   AND event.disposition = 'verified_applied'
                   AND event.outcome_code IS state.outcome_code
               )
             )",
            [batch_id.to_string()],
            |row| row.get(0),
        )?;
        if incomplete != 0 {
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

    /// Durably marks the local publication belonging to one download as applying.
    ///
    /// # Errors
    /// Rejects transfers outside the active batch, non-downloads, unclaimed
    /// transfers, or mutations already applying/committed.
    pub fn begin_transfer_local_publish(
        &mut self,
        operation_id: Uuid,
        now_unix_ms: u64,
    ) -> Result<()> {
        if operation_id.is_nil() {
            return Err(Error::TransferNotFound);
        }
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let active = load_change_batch(&transaction)?.ok_or(Error::NoActiveBatch)?;
        let transfer = load_transfer(&transaction, operation_id)?.ok_or(Error::TransferNotFound)?;
        if transfer.direction != TransferDirection::Download
            || !matches!(
                transfer.phase,
                TransferPhase::Running | TransferPhase::NeedsReconcile
            )
            || transfer.updated_at_unix_ms > now_unix_ms
        {
            return Err(Error::InvalidStateTransition);
        }
        let mutation_id = transfer_mutation_id(operation_id);
        let changed = transaction.execute(
            "UPDATE change_batch_mutations SET state = ?1
             WHERE batch_id = ?2 AND mutation_id = ?3 AND state = ?4",
            params![
                LocalMutationState::Applying.as_str(),
                active.batch_id.to_string(),
                mutation_id,
                LocalMutationState::Pending.as_str()
            ],
        )?;
        if changed != 1 {
            return match load_local_mutation_state(&transaction, active.batch_id, &mutation_id)? {
                Some(LocalMutationState::Applying)
                    if transfer.last_error_code.as_deref() == Some("reconcile_requested") =>
                {
                    transaction.commit()?;
                    Ok(())
                }
                Some(LocalMutationState::Applying | LocalMutationState::NeedsReconcile) => {
                    Err(Error::MutationNeedsReconcile)
                }
                Some(LocalMutationState::Committed) => Err(Error::InvalidStateTransition),
                Some(LocalMutationState::Pending) | None => Err(Error::UnknownMutation),
            };
        }
        let transfer_changed = transaction.execute(
            "UPDATE transfers SET updated_at_unix_ms = ?1
             WHERE operation_id = ?2 AND phase IN ('running', 'needs_reconcile')
               AND updated_at_unix_ms <= ?1",
            params![now, operation_id.to_string()],
        )?;
        if transfer_changed != 1 {
            return Err(Error::InvalidStateTransition);
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
        if !is_legacy_v3_dependency(&self.connection, batch_id, mutation_id)? {
            return Err(Error::InvalidStateTransition);
        }
        if is_transfer_backed_mutation(&self.connection, batch_id, mutation_id)? {
            return Err(Error::InvalidStateTransition);
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
            Some(LocalMutationState::Applying | LocalMutationState::NeedsReconcile) => {
                Err(Error::MutationNeedsReconcile)
            }
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
        if !is_legacy_v3_dependency(&self.connection, batch_id, mutation_id)? {
            return Err(Error::InvalidStateTransition);
        }
        if is_transfer_backed_mutation(&self.connection, batch_id, mutation_id)? {
            return Err(Error::InvalidStateTransition);
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
                Some(
                    LocalMutationState::Applying
                    | LocalMutationState::NeedsReconcile
                    | LocalMutationState::Pending
                    | LocalMutationState::Committed,
                ) => Err(Error::InvalidStateTransition),
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
        if !is_legacy_v3_dependency(&self.connection, batch_id, mutation_id)? {
            return Err(Error::InvalidStateTransition);
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
        if transfer_backed_mutation_count(&transaction, batch_id)? != 0 {
            return Err(Error::InvalidStateTransition);
        }
        if typed_r3_dependency_count(&transaction, batch_id)? != 0 {
            return Err(Error::InvalidStateTransition);
        }
        if legacy_v3_dependency_count(&transaction, batch_id)? != 0 {
            return Err(Error::LocalMutationIncomplete);
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

    /// Advances a transfer-coupled cursor only after exact completed evidence exists.
    ///
    /// A zero-mutation metadata page is valid. Every declared mutation on a non-empty
    /// page must map to a completed download, committed history, exact remote revision,
    /// and matching base fields on the resolved remote entry.
    ///
    /// # Errors
    /// Rejects missing/partial evidence, a different batch, or a changed cursor.
    pub fn commit_transfer_change_batch(&mut self, batch_id: Uuid, now_unix_ms: u64) -> Result<()> {
        let now = u64_to_i64(now_unix_ms)?;
        let transaction = self.connection.transaction()?;
        let batch = load_change_batch(&transaction)?.ok_or(Error::NoActiveBatch)?;
        if batch.batch_id != batch_id {
            return Err(Error::NoActiveBatch);
        }
        if typed_r3_dependency_count(&transaction, batch_id)? != 0 {
            return Err(Error::InvalidStateTransition);
        }
        if legacy_v3_dependency_count(&transaction, batch_id)? != 0 {
            return Err(Error::LocalMutationIncomplete);
        }
        if batch.applying_mutations != 0 || batch.declared_mutations != batch.committed_mutations {
            return Err(Error::LocalMutationIncomplete);
        }
        let incomplete: i64 = transaction.query_row(
            "SELECT COUNT(*)
             FROM change_batch_mutations AS mutation
             LEFT JOIN transfers AS transfer ON transfer.operation_id = mutation.mutation_id
             LEFT JOIN transfer_history AS history
               ON history.operation_id = transfer.operation_id
             LEFT JOIN remote_entries AS remote
               ON remote.file_id = transfer.remote_file_id
             WHERE mutation.batch_id = ?1 AND (
               mutation.state != 'committed' OR transfer.direction != 'download'
               OR transfer.phase != 'completed' OR history.operation_id IS NULL
               OR transfer.remote_file_id IS NULL OR transfer.base_reference IS NULL
               OR transfer.verified_local_revision IS NULL
               OR transfer.verified_remote_revision IS NULL
               OR transfer.expected_remote_revision IS NULL
               OR transfer.expected_remote_revision != transfer.verified_remote_revision
               OR remote.file_id IS NULL
               OR remote.parent_id != transfer.remote_parent_id
               OR remote.portable_path != transfer.portable_path OR remote.kind != 'file'
               OR remote.remote_revision != transfer.verified_remote_revision
               OR remote.base_local_revision != transfer.verified_local_revision
               OR remote.base_remote_revision != transfer.verified_remote_revision
               OR remote.base_content_hash != transfer.sha256
               OR remote.base_byte_length != transfer.byte_length
               OR (remote.content_hash_algorithm = 'sha256' AND
                   (remote.content_hash IS NULL OR remote.content_hash != transfer.sha256))
             )",
            [batch_id.to_string()],
            |row| row.get(0),
        )?;
        if incomplete != 0 {
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
        if transfer_backed_mutation_count(&transaction, batch_id)? != 0 {
            return Err(Error::MutationNeedsReconcile);
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
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE sync_jobs SET state = ?1, last_error_code = 'interrupted_unknown_outcome'
             WHERE state = ?2",
            params![
                JobState::NeedsReconcile.as_str(),
                JobState::Running.as_str()
            ],
        )?;
        transaction.execute(
            "UPDATE transfers
             SET phase = ?1, last_error_code = 'interrupted_unknown_outcome'
             WHERE phase = ?2",
            params![
                TransferPhase::NeedsReconcile.as_str(),
                TransferPhase::Running.as_str()
            ],
        )?;
        let running = {
            let mut statement = transaction.prepare(
                "SELECT operation_id, attempt_number, state_version, updated_at_unix_ms
                 FROM mutation_state WHERE phase = 'running' ORDER BY operation_id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        for (operation_id, attempt, version, updated_at) in running {
            let operation_id = parse_uuid(&operation_id)?;
            let attempt = u32::try_from(attempt).map_err(|_| Error::InvalidSchema)?;
            let version = u64::try_from(version).map_err(|_| Error::InvalidSchema)?;
            let occurred_at = u64::try_from(updated_at).map_err(|_| Error::InvalidSchema)?;
            let evidence =
                interrupted_mutation_evidence(operation_id, attempt, version, occurred_at);
            insert_mutation_evidence(&transaction, &evidence)?;
            let next = MutationState {
                operation_id,
                phase: MutationPhase::NeedsReconcile,
                attempt_number: attempt,
                state_version: version + 1,
                disposition: Some(MutationDisposition::NeedsReconcile),
                next_attempt_at_unix_ms: None,
                retry_mode: None,
                resume_reference: None,
                last_evidence_id: Some(evidence.evidence_id),
                outcome_code: evidence.outcome_code.clone(),
                updated_at_unix_ms: occurred_at,
            };
            update_mutation_state(&transaction, &next, version)?;
            insert_mutation_event(
                &transaction,
                operation_id,
                attempt,
                version + 1,
                MutationPhase::NeedsReconcile,
                Some(MutationDisposition::NeedsReconcile),
                Some(evidence.evidence_id),
                evidence.outcome_code.as_deref(),
                occurred_at,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

fn validate_mutation_intent(intent: &MutationIntent) -> Result<()> {
    if intent.operation_id.is_nil() {
        return Err(Error::MutationCollision);
    }
    validate_redacted_code(&intent.operation_marker)?;
    validate_revision(&intent.intent_fingerprint)?;
    if intent.intent_fingerprint != intent.canonical_fingerprint() {
        return Err(Error::InvalidTransferEvidence);
    }
    for value in [
        intent.account_id.as_deref(),
        intent.remote_root_id.as_deref(),
        intent.remote_file_id.as_deref(),
        intent.source_parent_id.as_deref(),
        intent.destination_parent_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_remote_id(value)?;
    }
    for value in [
        intent.source_path.as_deref(),
        intent.destination_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_content_path(value)?;
    }
    for value in [
        intent.expected_local_revision.as_deref(),
        intent.expected_remote_revision.as_deref(),
        intent.base_local_revision.as_deref(),
        intent.base_remote_revision.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_remote_id(value)?;
    }
    for value in [
        intent.local_object_id.as_deref(),
        intent.base_reference.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_private_reference(value)?;
    }
    validate_hash_size_pair(intent.base_sha256.as_deref(), intent.base_byte_length)?;
    validate_hash_size_pair(
        intent.expected_local_sha256.as_deref(),
        intent.expected_local_byte_length,
    )?;
    validate_hash_size_pair(
        intent.expected_remote_sha256.as_deref(),
        intent.expected_remote_byte_length,
    )?;
    u64_to_i64(intent.registered_at_unix_ms)?;
    if intent.account_id.is_some() != intent.remote_root_id.is_some() {
        return Err(Error::InvalidTransferEvidence);
    }
    if intent.operation_kind == MutationOperationKind::RemoteExistingBlocked
        && (intent.account_id.is_none()
            || intent.remote_root_id.is_none()
            || intent.remote_file_id.is_none()
            || intent.source_parent_id.is_none()
            || intent.source_path.is_none()
            || intent.expected_remote_revision.is_none())
    {
        return Err(Error::InvalidTransferEvidence);
    }
    if intent.operation_kind == MutationOperationKind::RemoteExistingBlocked
        && !matches!(
            (
                intent.base_local_revision.as_deref(),
                intent.base_remote_revision.as_deref(),
                intent.base_sha256.as_deref(),
                intent.base_byte_length,
            ),
            (None, None, None, None) | (Some(_), Some(_), Some(_), Some(_))
        )
    {
        // A blocked remote-existing intent may have no usable base after a
        // fail-closed migration, but it must never persist a partial base.
        return Err(Error::InvalidTransferEvidence);
    }
    Ok(())
}

fn validate_hash_size_pair(hash: Option<&str>, size: Option<u64>) -> Result<()> {
    match (hash, size) {
        (None, None) => Ok(()),
        (Some(hash), Some(size)) => {
            validate_revision(hash)?;
            u64_to_i64(size).map(|_| ())
        }
        _ => Err(Error::InvalidTransferEvidence),
    }
}

fn canonical_fingerprint<const N: usize>(
    domain: &str,
    fields: [(&str, Option<String>); N],
) -> String {
    let mut digest = Sha256::new();
    append_canonical_bytes(&mut digest, domain.as_bytes());
    for (name, value) in fields {
        append_canonical_bytes(&mut digest, name.as_bytes());
        match value {
            Some(value) => {
                digest.update([1]);
                append_canonical_bytes(&mut digest, value.as_bytes());
            }
            None => digest.update([0]),
        }
    }
    format!("{:x}", digest.finalize())
}

fn append_canonical_bytes(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

struct MutationEvidenceExpectation {
    account_id: Option<String>,
    remote_root_id: Option<String>,
    remote_file_id: Option<String>,
    source_parent_id: Option<String>,
    destination_parent_id: Option<String>,
    source_path: Option<String>,
    destination_path: Option<String>,
    expected_local_revision: Option<String>,
    expected_remote_revision: Option<String>,
    expected_local_sha256: Option<String>,
    expected_local_byte_length: Option<u64>,
    expected_remote_sha256: Option<String>,
    expected_remote_byte_length: Option<u64>,
    operation_marker: String,
}

fn load_mutation_evidence_expectation(
    connection: &Connection,
    operation_id: Uuid,
) -> Result<Option<MutationEvidenceExpectation>> {
    connection
        .query_row(
            "SELECT account_id, remote_root_id, remote_file_id, source_parent_id,
                    destination_parent_id, source_path, destination_path, expected_local_revision,
                    expected_remote_revision, expected_local_sha256, expected_local_byte_length,
                    expected_remote_sha256, expected_remote_byte_length, operation_marker
             FROM mutation_intents WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| {
                Ok(MutationEvidenceExpectation {
                    account_id: row.get(0)?,
                    remote_root_id: row.get(1)?,
                    remote_file_id: row.get(2)?,
                    source_parent_id: row.get(3)?,
                    destination_parent_id: row.get(4)?,
                    source_path: row.get(5)?,
                    destination_path: row.get(6)?,
                    expected_local_revision: row.get(7)?,
                    expected_remote_revision: row.get(8)?,
                    expected_local_sha256: row.get(9)?,
                    expected_local_byte_length: row
                        .get::<_, Option<i64>>(10)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(10, 0))?,
                    expected_remote_sha256: row.get(11)?,
                    expected_remote_byte_length: row
                        .get::<_, Option<i64>>(12)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(12, 0))?,
                    operation_marker: row.get(13)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn require_expected_match<T: Eq>(expected: Option<&T>, observed: Option<&T>) -> Result<()> {
    if expected.is_some() && expected != observed {
        return Err(Error::InvalidTransferEvidence);
    }
    Ok(())
}

fn validate_verified_applied_evidence(
    connection: &Connection,
    evidence: &MutationVerificationEvidence,
) -> Result<()> {
    if evidence.capture_phase != MutationEvidenceCapturePhase::PostVerify
        || evidence.forbidden_side_effect
    {
        return Err(Error::InvalidTransferEvidence);
    }
    let intent = load_mutation_evidence_expectation(connection, evidence.operation_id)?
        .ok_or(Error::MutationNotFound)?;
    let expected_parent = intent
        .destination_parent_id
        .as_ref()
        .or(intent.source_parent_id.as_ref());
    let expected_path = intent
        .destination_path
        .as_ref()
        .or(intent.source_path.as_ref());
    require_expected_match(
        intent.account_id.as_ref(),
        evidence.observed_account_id.as_ref(),
    )?;
    require_expected_match(
        intent.remote_root_id.as_ref(),
        evidence.observed_remote_root_id.as_ref(),
    )?;
    require_expected_match(
        intent.remote_file_id.as_ref(),
        evidence.observed_remote_file_id.as_ref(),
    )?;
    require_expected_match(expected_parent, evidence.observed_parent_id.as_ref())?;
    require_expected_match(expected_path, evidence.observed_path.as_ref())?;
    require_expected_match(
        intent.expected_local_revision.as_ref(),
        evidence.observed_local_revision.as_ref(),
    )?;
    require_expected_match(
        intent.expected_remote_revision.as_ref(),
        evidence.observed_remote_revision.as_ref(),
    )?;
    require_expected_match(
        intent.expected_local_sha256.as_ref(),
        evidence.observed_sha256.as_ref(),
    )?;
    require_expected_match(
        intent.expected_local_byte_length.as_ref(),
        evidence.observed_byte_length.as_ref(),
    )?;
    require_expected_match(
        intent.expected_remote_sha256.as_ref(),
        evidence.observed_sha256.as_ref(),
    )?;
    require_expected_match(
        intent.expected_remote_byte_length.as_ref(),
        evidence.observed_byte_length.as_ref(),
    )?;
    if evidence.observed_operation_marker.as_deref() != Some(intent.operation_marker.as_str()) {
        return Err(Error::InvalidTransferEvidence);
    }
    Ok(())
}

fn validate_mutation_evidence(evidence: &MutationVerificationEvidence) -> Result<()> {
    if evidence.evidence_id.is_nil() || evidence.operation_id.is_nil() {
        return Err(Error::InvalidTransferEvidence);
    }
    u64_to_i64(u64::from(evidence.attempt_number))?;
    validate_revision(&evidence.evidence_fingerprint)?;
    if evidence.evidence_fingerprint != evidence.canonical_fingerprint() {
        return Err(Error::InvalidTransferEvidence);
    }
    u64_to_i64(evidence.captured_at_unix_ms)?;
    if let Some(code) = &evidence.outcome_code {
        validate_redacted_code(code)?;
    }
    if evidence.disposition == MutationDisposition::NeedsReconcile
        && evidence.outcome_code.is_none()
    {
        return Err(Error::InvalidTransferEvidence);
    }
    for value in [
        evidence.observed_account_id.as_deref(),
        evidence.observed_remote_root_id.as_deref(),
        evidence.observed_remote_file_id.as_deref(),
        evidence.observed_parent_id.as_deref(),
        evidence.observed_local_revision.as_deref(),
        evidence.observed_remote_revision.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_remote_id(value)?;
    }
    if let Some(path) = &evidence.observed_path {
        validate_content_path(path)?;
    }
    validate_hash_size_pair(
        evidence.observed_sha256.as_deref(),
        evidence.observed_byte_length,
    )?;
    if let Some(marker) = &evidence.observed_operation_marker {
        validate_redacted_code(marker)?;
    }
    if let Some(offset) = evidence.verified_received_byte_offset {
        u64_to_i64(offset)?;
    }
    if let Some(reference) = &evidence.resume_reference {
        validate_private_reference(reference)?;
    }
    Ok(())
}

fn validate_conflict_evidence(evidence: &ConflictEvidence) -> Result<()> {
    if evidence.operation_id.is_nil() {
        return Err(Error::MutationCollision);
    }
    for value in [
        evidence.conflict_id.as_str(),
        evidence.stable_cell_id.as_str(),
        evidence.local_state_code.as_str(),
        evidence.remote_state_code.as_str(),
        evidence.content_class.as_str(),
        evidence.lineage_state.as_str(),
        evidence.classification_code.as_str(),
        evidence.ambiguity_reason.as_str(),
        evidence.evidence_sufficiency.as_str(),
        evidence.naming_version.as_str(),
        evidence.normalized_collision_key.as_str(),
    ] {
        validate_redacted_code(value)?;
    }
    validate_remote_id(&evidence.target_parent_id)?;
    if let Some(value) = &evidence.explanation_code {
        validate_redacted_code(value)?;
    }
    if let Some(value) = &evidence.device_alias {
        validate_redacted_code(value)?;
    }
    if let Some(operation_id) = evidence.conflict_copy_operation_id {
        if operation_id.is_nil() {
            return Err(Error::MutationCollision);
        }
    }
    for evidence_id in [
        evidence.base_evidence_id,
        evidence.local_evidence_id,
        evidence.remote_evidence_id,
    ]
    .into_iter()
    .flatten()
    {
        if evidence_id.is_nil() {
            return Err(Error::InvalidTransferEvidence);
        }
    }
    validate_hash_size_pair(evidence.base_sha256.as_deref(), evidence.base_byte_length)?;
    validate_hash_size_pair(evidence.local_sha256.as_deref(), evidence.local_byte_length)?;
    validate_hash_size_pair(
        evidence.remote_sha256.as_deref(),
        evidence.remote_byte_length,
    )?;
    validate_hash_size_pair(
        evidence.expected_conflict_copy_sha256.as_deref(),
        evidence.expected_conflict_copy_byte_length,
    )?;
    if evidence.conflict_copy_operation_id.is_some()
        && evidence.expected_conflict_copy_sha256.is_none()
    {
        return Err(Error::InvalidTransferEvidence);
    }
    validate_revision(&evidence.evidence_fingerprint)?;
    if evidence.evidence_fingerprint != evidence.canonical_fingerprint() {
        return Err(Error::InvalidTransferEvidence);
    }
    u64_to_i64(evidence.captured_at_unix_ms)?;
    Ok(())
}

type MutationTransitionTarget = (
    MutationPhase,
    MutationDisposition,
    Option<u64>,
    Option<MutationRetryMode>,
    Option<String>,
);

fn transition_target(
    current: MutationPhase,
    evidence: &MutationVerificationEvidence,
    transition: &MutationOutcomeTransition,
) -> Result<MutationTransitionTarget> {
    match transition {
        MutationOutcomeTransition::VerifiedApplied
            if matches!(
                current,
                MutationPhase::Running | MutationPhase::NeedsReconcile
            ) && evidence.disposition == MutationDisposition::VerifiedApplied
                && evidence.capture_phase == MutationEvidenceCapturePhase::PostVerify
                && !evidence.forbidden_side_effect =>
        {
            Ok((
                MutationPhase::Completed,
                MutationDisposition::VerifiedApplied,
                None,
                None,
                None,
            ))
        }
        MutationOutcomeTransition::NeedsReconcile
            if current == MutationPhase::Running
                && evidence.disposition == MutationDisposition::NeedsReconcile
                && evidence.outcome_code.is_some() =>
        {
            Ok((
                MutationPhase::NeedsReconcile,
                MutationDisposition::NeedsReconcile,
                None,
                None,
                None,
            ))
        }
        _ => Err(Error::InvalidStateTransition),
    }
}

fn insert_mutation_intent(transaction: &Transaction<'_>, intent: &MutationIntent) -> Result<()> {
    transaction.execute(
        "INSERT INTO mutation_intents(
            operation_id, operation_kind, account_id, remote_root_id, remote_file_id,
            source_parent_id, destination_parent_id, local_object_id, source_path,
            destination_path, expected_local_revision, expected_remote_revision,
            base_reference, base_local_revision, base_remote_revision, base_sha256,
            base_byte_length, expected_local_sha256, expected_local_byte_length,
            expected_remote_sha256, expected_remote_byte_length, operation_marker,
            intent_fingerprint, registered_at_unix_ms
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
         )",
        params![
            intent.operation_id.to_string(),
            intent.operation_kind.as_str(),
            intent.account_id,
            intent.remote_root_id,
            intent.remote_file_id,
            intent.source_parent_id,
            intent.destination_parent_id,
            intent.local_object_id,
            intent.source_path,
            intent.destination_path,
            intent.expected_local_revision,
            intent.expected_remote_revision,
            intent.base_reference,
            intent.base_local_revision,
            intent.base_remote_revision,
            intent.base_sha256,
            intent.base_byte_length.map(u64_to_i64).transpose()?,
            intent.expected_local_sha256,
            intent
                .expected_local_byte_length
                .map(u64_to_i64)
                .transpose()?,
            intent.expected_remote_sha256,
            intent
                .expected_remote_byte_length
                .map(u64_to_i64)
                .transpose()?,
            intent.operation_marker,
            intent.intent_fingerprint,
            u64_to_i64(intent.registered_at_unix_ms)?
        ],
    )?;
    Ok(())
}

fn insert_mutation_state(transaction: &Transaction<'_>, state: &MutationState) -> Result<()> {
    transaction.execute(
        "INSERT INTO mutation_state(
            operation_id, phase, attempt_number, state_version, disposition,
            next_attempt_at_unix_ms, retry_mode, resume_reference, last_evidence_id,
            outcome_code, updated_at_unix_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            state.operation_id.to_string(),
            state.phase.as_str(),
            i64::from(state.attempt_number),
            u64_to_i64(state.state_version)?,
            state.disposition.map(MutationDisposition::as_str),
            state.next_attempt_at_unix_ms.map(u64_to_i64).transpose()?,
            state.retry_mode.map(MutationRetryMode::as_str),
            state.resume_reference,
            state.last_evidence_id.map(|id| id.to_string()),
            state.outcome_code,
            u64_to_i64(state.updated_at_unix_ms)?
        ],
    )?;
    Ok(())
}

fn update_mutation_state(
    transaction: &Transaction<'_>,
    state: &MutationState,
    expected_version: u64,
) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE mutation_state
         SET phase = ?1, attempt_number = ?2, state_version = ?3, disposition = ?4,
             next_attempt_at_unix_ms = ?5, retry_mode = ?6, resume_reference = ?7,
             last_evidence_id = ?8, outcome_code = ?9, updated_at_unix_ms = ?10
         WHERE operation_id = ?11 AND state_version = ?12",
        params![
            state.phase.as_str(),
            i64::from(state.attempt_number),
            u64_to_i64(state.state_version)?,
            state.disposition.map(MutationDisposition::as_str),
            state.next_attempt_at_unix_ms.map(u64_to_i64).transpose()?,
            state.retry_mode.map(MutationRetryMode::as_str),
            state.resume_reference,
            state.last_evidence_id.map(|id| id.to_string()),
            state.outcome_code,
            u64_to_i64(state.updated_at_unix_ms)?,
            state.operation_id.to_string(),
            u64_to_i64(expected_version)?
        ],
    )?;
    if changed != 1 {
        return Err(Error::MutationStateVersionMismatch);
    }
    Ok(())
}

fn insert_mutation_evidence(
    transaction: &Transaction<'_>,
    evidence: &MutationVerificationEvidence,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO mutation_verification_evidence(
            evidence_id, operation_id, attempt_number, capture_phase, disposition, outcome_code,
            observed_account_id, observed_remote_root_id, observed_remote_file_id,
            observed_parent_id, observed_path, observed_local_revision, observed_remote_revision,
            observed_sha256, observed_byte_length, observed_operation_marker,
            forbidden_side_effect, verified_received_byte_offset, resume_reference,
            evidence_fingerprint, captured_at_unix_ms
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18, ?19, ?20, ?21
         )",
        params![
            evidence.evidence_id.to_string(),
            evidence.operation_id.to_string(),
            i64::from(evidence.attempt_number),
            evidence.capture_phase.as_str(),
            evidence.disposition.as_str(),
            evidence.outcome_code,
            evidence.observed_account_id,
            evidence.observed_remote_root_id,
            evidence.observed_remote_file_id,
            evidence.observed_parent_id,
            evidence.observed_path,
            evidence.observed_local_revision,
            evidence.observed_remote_revision,
            evidence.observed_sha256,
            evidence.observed_byte_length.map(u64_to_i64).transpose()?,
            evidence.observed_operation_marker,
            i64::from(evidence.forbidden_side_effect),
            evidence
                .verified_received_byte_offset
                .map(u64_to_i64)
                .transpose()?,
            evidence.resume_reference,
            evidence.evidence_fingerprint,
            u64_to_i64(evidence.captured_at_unix_ms)?
        ],
    )?;
    Ok(())
}

fn insert_conflict_evidence(
    transaction: &Transaction<'_>,
    evidence: &ConflictEvidence,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO conflict_evidence(
            conflict_id, operation_id, stable_cell_id, local_state_code, remote_state_code,
            content_class, lineage_state, classification_code, ambiguity_reason,
            evidence_sufficiency, conflict_copy_operation_id, base_evidence_id,
            local_evidence_id, remote_evidence_id, base_sha256, base_byte_length,
            local_sha256, local_byte_length, remote_sha256, remote_byte_length,
            naming_version, normalized_collision_key, target_parent_id,
            expected_conflict_copy_sha256, expected_conflict_copy_byte_length,
            explanation_code, device_alias, evidence_fingerprint, captured_at_unix_ms
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
            ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29
         )",
        params![
            evidence.conflict_id,
            evidence.operation_id.to_string(),
            evidence.stable_cell_id,
            evidence.local_state_code,
            evidence.remote_state_code,
            evidence.content_class,
            evidence.lineage_state,
            evidence.classification_code,
            evidence.ambiguity_reason,
            evidence.evidence_sufficiency,
            evidence
                .conflict_copy_operation_id
                .map(|value| value.to_string()),
            evidence.base_evidence_id.map(|value| value.to_string()),
            evidence.local_evidence_id.map(|value| value.to_string()),
            evidence.remote_evidence_id.map(|value| value.to_string()),
            evidence.base_sha256,
            evidence.base_byte_length.map(u64_to_i64).transpose()?,
            evidence.local_sha256,
            evidence.local_byte_length.map(u64_to_i64).transpose()?,
            evidence.remote_sha256,
            evidence.remote_byte_length.map(u64_to_i64).transpose()?,
            evidence.naming_version,
            evidence.normalized_collision_key,
            evidence.target_parent_id,
            evidence.expected_conflict_copy_sha256,
            evidence
                .expected_conflict_copy_byte_length
                .map(u64_to_i64)
                .transpose()?,
            evidence.explanation_code,
            evidence.device_alias,
            evidence.evidence_fingerprint,
            u64_to_i64(evidence.captured_at_unix_ms)?,
        ],
    )?;
    Ok(())
}

fn load_conflict_evidence_fingerprint(
    connection: &Connection,
    conflict_id: &str,
) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT evidence_fingerprint FROM conflict_evidence WHERE conflict_id = ?1",
            [conflict_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn evidence_belongs_to_operation(
    connection: &Connection,
    evidence_id: Uuid,
    operation_id: Uuid,
) -> Result<bool> {
    connection
        .query_row(
            "SELECT 1 FROM mutation_verification_evidence
             WHERE evidence_id = ?1 AND operation_id = ?2",
            params![evidence_id.to_string(), operation_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(Into::into)
}

#[allow(clippy::too_many_lines, clippy::type_complexity)]
fn load_conflict_evidence(
    connection: &Connection,
    conflict_id: &str,
) -> Result<Option<ConflictEvidence>> {
    let row = connection
        .query_row(
            "SELECT conflict_id, operation_id, stable_cell_id, local_state_code, remote_state_code,
                    content_class, lineage_state, classification_code, ambiguity_reason,
                    evidence_sufficiency, conflict_copy_operation_id, base_evidence_id,
                    local_evidence_id, remote_evidence_id, base_sha256, base_byte_length,
                    local_sha256, local_byte_length, remote_sha256, remote_byte_length,
                    naming_version, normalized_collision_key, target_parent_id,
                    expected_conflict_copy_sha256, expected_conflict_copy_byte_length,
                    explanation_code, device_alias, evidence_fingerprint, captured_at_unix_ms
             FROM conflict_evidence WHERE conflict_id = ?1",
            [conflict_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, Option<String>>(14)?,
                    row.get::<_, Option<i64>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, Option<i64>>(17)?,
                    row.get::<_, Option<String>>(18)?,
                    row.get::<_, Option<i64>>(19)?,
                    row.get::<_, String>(20)?,
                    row.get::<_, String>(21)?,
                    row.get::<_, String>(22)?,
                    row.get::<_, Option<String>>(23)?,
                    row.get::<_, Option<i64>>(24)?,
                    row.get::<_, Option<String>>(25)?,
                    row.get::<_, Option<String>>(26)?,
                    row.get::<_, String>(27)?,
                    row.get::<_, i64>(28)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        let (
            conflict_id,
            operation_id,
            stable_cell_id,
            local_state_code,
            remote_state_code,
            content_class,
            lineage_state,
            classification_code,
            ambiguity_reason,
            evidence_sufficiency,
            conflict_copy_operation_id,
            base_evidence_id,
            local_evidence_id,
            remote_evidence_id,
            base_sha256,
            base_byte_length,
            local_sha256,
            local_byte_length,
            remote_sha256,
            remote_byte_length,
            naming_version,
            normalized_collision_key,
            target_parent_id,
            expected_conflict_copy_sha256,
            expected_conflict_copy_byte_length,
            explanation_code,
            device_alias,
            evidence_fingerprint,
            captured_at_unix_ms,
        ) = row;
        Ok(ConflictEvidence {
            conflict_id,
            operation_id: parse_uuid(&operation_id)?,
            stable_cell_id,
            local_state_code,
            remote_state_code,
            content_class,
            lineage_state,
            classification_code,
            ambiguity_reason,
            evidence_sufficiency,
            conflict_copy_operation_id: parse_optional_uuid(conflict_copy_operation_id)?,
            base_evidence_id: parse_optional_uuid(base_evidence_id)?,
            local_evidence_id: parse_optional_uuid(local_evidence_id)?,
            remote_evidence_id: parse_optional_uuid(remote_evidence_id)?,
            base_sha256,
            base_byte_length: optional_i64_to_u64(base_byte_length)?,
            local_sha256,
            local_byte_length: optional_i64_to_u64(local_byte_length)?,
            remote_sha256,
            remote_byte_length: optional_i64_to_u64(remote_byte_length)?,
            naming_version,
            normalized_collision_key,
            target_parent_id,
            expected_conflict_copy_sha256,
            expected_conflict_copy_byte_length: optional_i64_to_u64(
                expected_conflict_copy_byte_length,
            )?,
            explanation_code,
            device_alias,
            evidence_fingerprint,
            captured_at_unix_ms: u64::try_from(captured_at_unix_ms)
                .map_err(|_| Error::InvalidSchema)?,
        })
    })
    .transpose()
}

fn parse_optional_uuid(value: Option<String>) -> Result<Option<Uuid>> {
    value.map(|value| parse_uuid(&value)).transpose()
}

fn optional_i64_to_u64(value: Option<i64>) -> Result<Option<u64>> {
    value
        .map(u64::try_from)
        .transpose()
        .map_err(|_| Error::InvalidSchema)
}

#[allow(clippy::too_many_arguments)]
fn insert_mutation_event(
    transaction: &Transaction<'_>,
    operation_id: Uuid,
    attempt_number: u32,
    state_version: u64,
    phase: MutationPhase,
    disposition: Option<MutationDisposition>,
    evidence_id: Option<Uuid>,
    outcome_code: Option<&str>,
    occurred_at_unix_ms: u64,
) -> Result<()> {
    if let Some(code) = outcome_code {
        validate_redacted_code(code)?;
    }
    transaction.execute(
        "INSERT INTO mutation_events(
            operation_id, attempt_number, state_version, phase, disposition, evidence_id,
            outcome_code, occurred_at_unix_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            operation_id.to_string(),
            i64::from(attempt_number),
            u64_to_i64(state_version)?,
            phase.as_str(),
            disposition.map(MutationDisposition::as_str),
            evidence_id.map(|id| id.to_string()),
            outcome_code,
            u64_to_i64(occurred_at_unix_ms)?
        ],
    )?;
    Ok(())
}

fn load_mutation_identity(
    connection: &Connection,
    operation_id: Uuid,
) -> Result<Option<(String, String)>> {
    connection
        .query_row(
            "SELECT intent_fingerprint, operation_marker FROM mutation_intents WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
}

fn mutation_marker_exists(connection: &Connection, marker: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM mutation_intents WHERE operation_marker = ?1",
            [marker],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn load_mutation_kind(
    connection: &Connection,
    operation_id: Uuid,
) -> Result<Option<(String, MutationOperationKind)>> {
    let row = connection
        .query_row(
            "SELECT operation_id, operation_kind FROM mutation_intents WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map_or(Ok(None), |(id, kind)| {
        Ok(Some((id, MutationOperationKind::parse(&kind)?)))
    })
}

fn load_mutation_state(
    connection: &Connection,
    operation_id: Uuid,
) -> Result<Option<MutationState>> {
    connection
        .query_row(
            "SELECT operation_id, phase, attempt_number, state_version, disposition,
                    next_attempt_at_unix_ms, retry_mode, resume_reference, last_evidence_id,
                    outcome_code, updated_at_unix_ms
             FROM mutation_state WHERE operation_id = ?1",
            [operation_id.to_string()],
            row_to_mutation_state,
        )
        .optional()?
        .map_or(Ok(None), |state| Ok(Some(state?)))
}

#[allow(clippy::unnecessary_wraps)]
fn row_to_mutation_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<MutationState>> {
    Ok((|| -> Result<MutationState> {
        let operation_id = parse_uuid(&row.get::<_, String>(0)?)?;
        let phase = MutationPhase::parse(&row.get::<_, String>(1)?)?;
        let attempt_number =
            u32::try_from(row.get::<_, i64>(2)?).map_err(|_| Error::InvalidSchema)?;
        let state_version =
            u64::try_from(row.get::<_, i64>(3)?).map_err(|_| Error::InvalidSchema)?;
        let disposition = match row.get::<_, Option<String>>(4)? {
            Some(value) => Some(MutationDisposition::parse(&value)?),
            None => None,
        };
        let next_attempt_at_unix_ms = match row.get::<_, Option<i64>>(5)? {
            Some(value) => Some(u64::try_from(value).map_err(|_| Error::InvalidSchema)?),
            None => None,
        };
        let retry_mode = match row.get::<_, Option<String>>(6)? {
            Some(value) => Some(MutationRetryMode::parse(&value)?),
            None => None,
        };
        let resume_reference = row.get(7)?;
        let last_evidence_id = match row.get::<_, Option<String>>(8)? {
            Some(value) => Some(parse_uuid(&value)?),
            None => None,
        };
        let outcome_code: Option<String> = row.get(9)?;
        if let Some(code) = &outcome_code {
            validate_redacted_code(code)?;
        }
        let updated_at_unix_ms =
            u64::try_from(row.get::<_, i64>(10)?).map_err(|_| Error::InvalidSchema)?;
        Ok(MutationState {
            operation_id,
            phase,
            attempt_number,
            state_version,
            disposition,
            next_attempt_at_unix_ms,
            retry_mode,
            resume_reference,
            last_evidence_id,
            outcome_code,
            updated_at_unix_ms,
        })
    })())
}

fn load_mutation_events(connection: &Connection, operation_id: Uuid) -> Result<Vec<MutationEvent>> {
    let mut statement = connection.prepare(
        "SELECT event_id, operation_id, attempt_number, state_version, phase, disposition,
                evidence_id, outcome_code, occurred_at_unix_ms
         FROM mutation_events WHERE operation_id = ?1 ORDER BY event_id",
    )?;
    let rows = statement.query_map([operation_id.to_string()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, i64>(8)?,
        ))
    })?;
    rows.map(|row| {
        let (
            event_id,
            operation_id,
            attempt_number,
            state_version,
            phase,
            disposition,
            evidence_id,
            outcome_code,
            occurred_at,
        ) = row?;
        if let Some(code) = &outcome_code {
            validate_redacted_code(code)?;
        }
        Ok(MutationEvent {
            event_id: u64::try_from(event_id).map_err(|_| Error::InvalidSchema)?,
            operation_id: parse_uuid(&operation_id)?,
            attempt_number: u32::try_from(attempt_number).map_err(|_| Error::InvalidSchema)?,
            state_version: u64::try_from(state_version).map_err(|_| Error::InvalidSchema)?,
            phase: MutationPhase::parse(&phase)?,
            disposition: match disposition {
                Some(value) => Some(MutationDisposition::parse(&value)?),
                None => None,
            },
            evidence_id: match evidence_id {
                Some(value) => Some(parse_uuid(&value)?),
                None => None,
            },
            outcome_code,
            occurred_at_unix_ms: u64::try_from(occurred_at).map_err(|_| Error::InvalidSchema)?,
        })
    })
    .collect()
}

fn interrupted_mutation_evidence(
    operation_id: Uuid,
    attempt_number: u32,
    _state_version: u64,
    captured_at_unix_ms: u64,
) -> MutationVerificationEvidence {
    let mut evidence = MutationVerificationEvidence {
        evidence_id: Uuid::new_v4(),
        operation_id,
        attempt_number,
        capture_phase: MutationEvidenceCapturePhase::Reconcile,
        disposition: MutationDisposition::NeedsReconcile,
        outcome_code: Some("interrupted_unknown_outcome".into()),
        observed_account_id: None,
        observed_remote_root_id: None,
        observed_remote_file_id: None,
        observed_parent_id: None,
        observed_path: None,
        observed_local_revision: None,
        observed_remote_revision: None,
        observed_sha256: None,
        observed_byte_length: None,
        observed_operation_marker: None,
        forbidden_side_effect: true,
        verified_received_byte_offset: None,
        resume_reference: None,
        evidence_fingerprint: String::new(),
        captured_at_unix_ms,
    };
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    evidence
}

fn create_or_open_storage_dir(
    parent: &Dir,
    name: impl AsRef<Path>,
    policy: PrivateStoragePolicy,
) -> Result<Dir> {
    match policy {
        PrivateStoragePolicy::Standard => Ok(private_fs::create_or_open_private_dir(parent, name)?),
        #[cfg(target_os = "android")]
        PrivateStoragePolicy::NativeAndroidNoBackup => Ok(
            private_fs::create_or_open_android_private_dir(parent, name)?,
        ),
    }
}

fn harden_new_storage_file(file: &cap_std::fs::File, policy: PrivateStoragePolicy) -> Result<()> {
    match policy {
        PrivateStoragePolicy::Standard => private_fs::set_private_file_permissions(file)?,
        #[cfg(target_os = "android")]
        PrivateStoragePolicy::NativeAndroidNoBackup => {
            private_fs::harden_android_new_file(file)?;
        }
    }
    Ok(())
}

fn verify_storage_file(file: &cap_std::fs::File, policy: PrivateStoragePolicy) -> Result<()> {
    match policy {
        PrivateStoragePolicy::Standard => private_fs::verify_private_file(file, 1)?,
        #[cfg(target_os = "android")]
        PrivateStoragePolicy::NativeAndroidNoBackup => {
            private_fs::inspect_android_held_file(file)?;
        }
    }
    Ok(())
}

fn open_storage_file(
    parent: &Dir,
    name: impl AsRef<Path>,
    policy: PrivateStoragePolicy,
) -> Result<()> {
    match policy {
        PrivateStoragePolicy::Standard => {
            private_fs::open_private_file(parent, name, 1)?;
        }
        #[cfg(target_os = "android")]
        PrivateStoragePolicy::NativeAndroidNoBackup => {
            private_fs::open_android_private_file(parent, name)?;
        }
    }
    Ok(())
}

fn acquire_sync_lease(
    vault_directory: &Dir,
    policy: PrivateStoragePolicy,
) -> Result<std::fs::File> {
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
        harden_new_storage_file(&lease, policy)?;
    }
    verify_storage_file(&lease, policy)?;
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
    let after_v1: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if after_v1 == 2 {
        if !schema_v2_is_valid(&transaction)? {
            return Err(Error::InvalidSchema);
        }
        migrate_v2_to_v3(&transaction)?;
    }
    let after_v2: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if after_v2 == 3 {
        if !schema_v3_is_valid(&transaction)? {
            return Err(Error::InvalidSchema);
        }
        migrate_v3_to_v4(&transaction)?;
    }
    let after_v3: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if after_v3 == 4 {
        if !schema_v4_is_valid(&transaction)? {
            return Err(Error::InvalidSchema);
        }
        migrate_v4_to_v5(&transaction)?;
    }
    if !schema_v5_is_valid(&transaction)? {
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

fn migrate_v2_to_v3(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(TRANSFERS_SCHEMA)?;
    transaction.execute_batch(TRANSFERS_DUE_INDEX_SCHEMA)?;
    transaction.execute_batch(TRANSFER_HISTORY_SCHEMA)?;
    transaction.pragma_update(None, "user_version", 3)?;
    Ok(())
}

fn migrate_v3_to_v4(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(MUTATION_INTENTS_SCHEMA)?;
    transaction.execute_batch(MUTATION_VERIFICATION_EVIDENCE_SCHEMA)?;
    transaction.execute_batch(MUTATION_STATE_SCHEMA)?;
    transaction.execute_batch(MUTATION_EVENTS_SCHEMA)?;
    transaction.execute_batch(CONFLICT_EVIDENCE_SCHEMA)?;
    transaction.execute_batch(
        "ALTER TABLE change_batch_mutations RENAME TO change_batch_mutations_v3;
         CREATE TABLE change_batch_mutations (
            batch_id TEXT NOT NULL,
            mutation_id TEXT NOT NULL,
            dependency_kind TEXT NOT NULL CHECK (dependency_kind IN ('mutation', 'merge_publication', 'conflict_copy_publication', 'base_publication', 'legacy_v3')),
            operation_id TEXT,
            committed_evidence_id TEXT,
            state TEXT NOT NULL CHECK (state IN ('pending', 'applying', 'needs_reconcile', 'committed')),
            PRIMARY KEY (batch_id, mutation_id),
            FOREIGN KEY (batch_id) REFERENCES change_batch(batch_id) ON DELETE CASCADE,
            FOREIGN KEY (operation_id) REFERENCES mutation_intents(operation_id),
            FOREIGN KEY (committed_evidence_id) REFERENCES mutation_verification_evidence(evidence_id),
            CHECK ((dependency_kind = 'legacy_v3' AND operation_id IS NULL AND committed_evidence_id IS NULL) OR (dependency_kind != 'legacy_v3' AND operation_id IS NOT NULL))
         );
         INSERT INTO change_batch_mutations(
            batch_id, mutation_id, dependency_kind, operation_id, committed_evidence_id, state
         )
         SELECT batch_id, mutation_id, 'legacy_v3', NULL, NULL,
                CASE state WHEN 'applying' THEN 'needs_reconcile' ELSE state END
         FROM change_batch_mutations_v3;
         DROP TABLE change_batch_mutations_v3;",
    )?;
    transaction.execute_batch(MUTATION_STATE_CLAIM_INDEX_SCHEMA)?;
    transaction.execute_batch(MUTATION_EVENTS_OPERATION_ATTEMPT_INDEX_SCHEMA)?;
    transaction.execute_batch(MUTATION_EVIDENCE_OPERATION_ATTEMPT_INDEX_SCHEMA)?;
    transaction.execute_batch(CONFLICT_EVIDENCE_STABLE_CELL_INDEX_SCHEMA)?;
    transaction.execute_batch(CONFLICT_EVIDENCE_COPY_INDEX_SCHEMA)?;
    transaction.execute_batch(MUTATION_INTENTS_NO_UPDATE_TRIGGER)?;
    transaction.execute_batch(MUTATION_INTENTS_NO_DELETE_TRIGGER)?;
    transaction.execute_batch(MUTATION_EVENTS_NO_UPDATE_TRIGGER)?;
    transaction.execute_batch(MUTATION_EVENTS_NO_DELETE_TRIGGER)?;
    transaction.execute_batch(MUTATION_EVIDENCE_NO_UPDATE_TRIGGER)?;
    transaction.execute_batch(MUTATION_EVIDENCE_NO_DELETE_TRIGGER)?;
    transaction.execute_batch(CONFLICT_EVIDENCE_NO_UPDATE_TRIGGER)?;
    transaction.execute_batch(CONFLICT_EVIDENCE_NO_DELETE_TRIGGER)?;
    transaction.pragma_update(None, "user_version", 4)?;
    Ok(())
}

fn migrate_v4_to_v5(transaction: &Transaction<'_>) -> Result<()> {
    // V4 retained a base hash without its exact byte length. It cannot be
    // upgraded by inference, so clear those incomplete bases atomically rather
    // than fabricating evidence or weakening the new pair invariant.
    transaction.execute_batch(
        "DROP INDEX remote_entries_path_idx;
         DROP INDEX remote_entries_preview_idx;
         ALTER TABLE remote_entries RENAME TO remote_entries_v4;",
    )?;
    transaction.execute_batch(REMOTE_ENTRIES_SCHEMA)?;
    transaction.execute_batch(
        "INSERT INTO remote_entries(
            file_id, parent_id, portable_path, kind, content_hash_algorithm,
            content_hash, remote_revision
         )
         SELECT file_id, parent_id, portable_path, kind, content_hash_algorithm,
                content_hash, remote_revision
         FROM remote_entries_v4;
         DROP TABLE remote_entries_v4;",
    )?;
    transaction.execute_batch(REMOTE_ENTRIES_INDEX_SCHEMA)?;
    transaction.execute_batch(REMOTE_ENTRIES_PREVIEW_INDEX_SCHEMA)?;
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
    transaction.pragma_update(None, "user_version", 2)?;
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
    if !schema_definitions_are_exact(connection, &SCHEMA_OBJECTS_V2)? {
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

fn schema_v3_is_valid(connection: &Connection) -> Result<bool> {
    if !schema_definitions_are_exact(connection, &SCHEMA_OBJECTS_V3)? {
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
        "transfer_history",
        "transfers",
        "vault_state",
    ];
    if tables.iter().map(String::as_str).ne(expected) {
        return Ok(false);
    }
    if !primary_schema_columns_are_valid_v2(connection)?
        || !transfer_schema_columns_are_valid(connection)?
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
        || !index_has_columns(
            connection,
            "transfers_due_idx",
            &[
                "phase",
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

fn schema_v4_is_valid(connection: &Connection) -> Result<bool> {
    if !schema_definitions_are_exact(connection, &SCHEMA_OBJECTS_V4)? {
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
        "conflict_evidence",
        "mutation_events",
        "mutation_intents",
        "mutation_state",
        "mutation_verification_evidence",
        "remote_entries",
        "scan_frontier",
        "sync_history",
        "sync_jobs",
        "transfer_history",
        "transfers",
        "vault_state",
    ];
    if tables.iter().map(String::as_str).ne(expected) {
        return Ok(false);
    }
    let foreign_key_errors: i64 =
        connection.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    Ok(foreign_key_errors == 0)
}

fn schema_v5_is_valid(connection: &Connection) -> Result<bool> {
    if !schema_definitions_are_exact(connection, &SCHEMA_OBJECTS)? {
        return Ok(false);
    }
    let foreign_key_errors: i64 =
        connection.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    Ok(foreign_key_errors == 0)
}

fn transfer_schema_columns_are_valid(connection: &Connection) -> Result<bool> {
    Ok(table_has_columns(
        connection,
        "transfers",
        &[
            ("operation_id", "TEXT", true, 1),
            ("direction", "TEXT", true, 0),
            ("portable_path", "TEXT", true, 0),
            ("remote_parent_id", "TEXT", true, 0),
            ("remote_file_id", "TEXT", false, 0),
            ("display_name", "TEXT", true, 0),
            ("expected_local_revision", "TEXT", false, 0),
            ("expected_remote_revision", "TEXT", false, 0),
            ("sha256", "TEXT", true, 0),
            ("byte_length", "INTEGER", true, 0),
            ("mime_class", "TEXT", true, 0),
            ("operation_marker", "TEXT", true, 0),
            ("stage_reference", "TEXT", false, 0),
            ("base_reference", "TEXT", false, 0),
            ("phase", "TEXT", true, 0),
            ("attempt_count", "INTEGER", true, 0),
            ("next_attempt_at_unix_ms", "INTEGER", true, 0),
            ("created_at_unix_ms", "INTEGER", true, 0),
            ("updated_at_unix_ms", "INTEGER", true, 0),
            ("last_error_code", "TEXT", false, 0),
            ("verified_local_revision", "TEXT", false, 0),
            ("verified_remote_revision", "TEXT", false, 0),
        ],
    )? && table_has_columns(
        connection,
        "transfer_history",
        &[
            ("event_id", "INTEGER", true, 1),
            ("operation_id", "TEXT", true, 0),
            ("outcome_code", "TEXT", true, 0),
            ("occurred_at_unix_ms", "INTEGER", true, 0),
        ],
    )?)
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

fn load_remote_entry(connection: &Connection, file_id: &str) -> Result<Option<RemoteEntry>> {
    let persisted: Option<PersistedRemoteEntry> = connection
        .query_row(
            "SELECT file_id, parent_id, portable_path, kind,
                    content_hash_algorithm, content_hash, remote_revision
             FROM remote_entries WHERE file_id = ?1",
            [file_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    persisted.map_or(Ok(None), |persisted| {
        let (
            file_id,
            parent_id,
            path,
            kind,
            content_hash_algorithm,
            content_hash,
            remote_revision,
        ) = persisted;
        let content_hash = match (content_hash_algorithm.as_deref(), content_hash) {
            (None, None) => None,
            (Some(algorithm), Some(hex)) => Some(RemoteContentHash::new(
                match algorithm {
                    "md5" => RemoteHashAlgorithm::Md5,
                    "sha1" => RemoteHashAlgorithm::Sha1,
                    "sha256" => RemoteHashAlgorithm::Sha256,
                    _ => return Err(Error::InvalidSchema),
                },
                hex,
            )?),
            _ => return Err(Error::InvalidSchema),
        };
        let entry = RemoteEntry {
            file_id,
            parent_id,
            path,
            kind: match kind.as_str() {
                "file" => RemoteEntryKind::File,
                "folder" => RemoteEntryKind::Folder,
                _ => return Err(Error::InvalidSchema),
            },
            content_hash,
            remote_revision,
        };
        entry.validate()?;
        Ok(Some(entry))
    })
}

fn validate_new_transfer(transfer: &TransferRecord) -> Result<()> {
    transfer.validate()?;
    if transfer.phase != TransferPhase::Pending
        || transfer.attempt_count != 0
        || transfer.last_error_code.is_some()
        || transfer.verified_local_revision.is_some()
        || transfer.verified_remote_revision.is_some()
    {
        return Err(Error::InvalidTransferEvidence);
    }
    Ok(())
}

fn validate_resolved_transfer_changes(
    connection: &Connection,
    changes: &[RemoteChange],
    downloads: &[TransferRecord],
) -> Result<()> {
    let mut changed_ids = BTreeSet::new();
    let mut required_downloads = BTreeSet::new();
    for change in changes {
        match change {
            RemoteChange::Removed { .. } => return Err(Error::UnsupportedTransferChange),
            RemoteChange::Upsert(entry) => {
                if !changed_ids.insert(entry.file_id.as_str()) {
                    return Err(Error::InvalidRemoteEntry);
                }
                let existing = load_remote_entry(connection, &entry.file_id)?;
                if existing.as_ref().is_some_and(|previous| {
                    previous.path != entry.path
                        || previous.parent_id != entry.parent_id
                        || previous.kind != entry.kind
                }) {
                    return Err(Error::UnsupportedTransferChange);
                }
                let path_owner: Option<String> = connection
                    .query_row(
                        "SELECT file_id FROM remote_entries
                         WHERE portable_path = ?1 AND file_id != ?2 LIMIT 1",
                        params![entry.path, entry.file_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if path_owner.is_some() {
                    return Err(Error::UnsupportedTransferChange);
                }
                if entry.kind == RemoteEntryKind::File
                    && existing.as_ref().is_none_or(|previous| {
                        previous.remote_revision != entry.remote_revision
                            || previous.content_hash != entry.content_hash
                    })
                {
                    required_downloads.insert(entry.file_id.as_str());
                }
            }
        }
    }

    let mut supplied_downloads = BTreeSet::new();
    for transfer in downloads {
        let file_id = transfer
            .remote_file_id
            .as_deref()
            .ok_or(Error::TransferChangeMismatch)?;
        if !supplied_downloads.insert(file_id) {
            return Err(Error::TransferChangeMismatch);
        }
        let entry = changes
            .iter()
            .find_map(|change| match change {
                RemoteChange::Upsert(entry) if entry.file_id == file_id => Some(entry),
                _ => None,
            })
            .ok_or(Error::TransferChangeMismatch)?;
        if entry.kind != RemoteEntryKind::File
            || transfer.portable_path != entry.path
            || transfer.remote_parent_id != entry.parent_id
            || transfer.expected_remote_revision.as_deref() != Some(entry.remote_revision.as_str())
            || entry.content_hash.as_ref().is_some_and(|hash| {
                hash.algorithm == RemoteHashAlgorithm::Sha256 && hash.hex != transfer.sha256
            })
        {
            return Err(Error::TransferChangeMismatch);
        }
    }
    if supplied_downloads != required_downloads {
        return Err(Error::TransferChangeMismatch);
    }
    Ok(())
}

fn update_remote_base_if_present(
    transaction: &Transaction<'_>,
    transfer: &TransferRecord,
    completion: &TransferCompletion,
) -> Result<()> {
    let exists = transaction
        .query_row(
            "SELECT 1 FROM remote_entries WHERE file_id = ?1",
            [&completion.remote_file_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(());
    }
    let metadata_changed = transaction.execute(
        "UPDATE remote_entries
         SET base_local_revision = ?1, base_remote_revision = ?2,
             base_content_hash = ?3, base_byte_length = ?4
         WHERE file_id = ?5 AND parent_id = ?6 AND portable_path = ?7
           AND kind = 'file' AND remote_revision = ?2
           AND (content_hash_algorithm IS NULL OR content_hash_algorithm != 'sha256'
                OR content_hash = ?3)",
        params![
            completion.local_revision,
            completion.remote_revision,
            transfer.sha256,
            u64_to_i64(transfer.byte_length)?,
            completion.remote_file_id,
            transfer.remote_parent_id,
            transfer.portable_path
        ],
    )?;
    if metadata_changed != 1 {
        return Err(Error::TransferChangeMismatch);
    }
    Ok(())
}

fn commit_transfer_mutation(transaction: &Transaction<'_>, mutation_id: &str) -> Result<()> {
    let mutation_changed = transaction.execute(
        "UPDATE change_batch_mutations SET state = ?1
         WHERE mutation_id = ?2 AND state = ?3
           AND batch_id = (SELECT batch_id FROM change_batch WHERE singleton = 1)",
        params![
            LocalMutationState::Committed.as_str(),
            mutation_id,
            LocalMutationState::Applying.as_str()
        ],
    )?;
    if mutation_changed != 1 {
        return Err(Error::InvalidStateTransition);
    }
    Ok(())
}

fn register_transfer_in_transaction(
    transaction: &Transaction<'_>,
    transfer: &TransferRecord,
) -> Result<()> {
    if let Some(existing) = load_transfer(transaction, transfer.operation_id)? {
        if !existing.same_registration(transfer) || existing.phase == TransferPhase::Completed {
            return Err(Error::TransferCollision);
        }
        return Ok(());
    }
    let marker_owner: Option<String> = transaction
        .query_row(
            "SELECT operation_id FROM transfers WHERE operation_marker = ?1",
            [&transfer.operation_marker],
            |row| row.get(0),
        )
        .optional()?;
    if marker_owner.is_some() {
        return Err(Error::TransferCollision);
    }
    insert_transfer(transaction, transfer)
}

fn transfer_mutation_id(operation_id: Uuid) -> String {
    operation_id.to_string()
}

fn active_transfer_mutation_state(
    connection: &Connection,
    mutation_id: &str,
) -> Result<Option<LocalMutationState>> {
    let Some(batch) = load_change_batch(connection)? else {
        return Ok(None);
    };
    load_local_mutation_state(connection, batch.batch_id, mutation_id)
}

fn is_transfer_backed_mutation(
    connection: &Connection,
    batch_id: Uuid,
    mutation_id: &str,
) -> Result<bool> {
    let found = connection
        .query_row(
            "SELECT 1
             FROM change_batch_mutations AS mutation
             JOIN transfers AS transfer ON transfer.operation_id = mutation.mutation_id
             WHERE mutation.batch_id = ?1 AND mutation.mutation_id = ?2",
            params![batch_id.to_string(), mutation_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(found)
}

fn is_legacy_v3_dependency(
    connection: &Connection,
    batch_id: Uuid,
    mutation_id: &str,
) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT dependency_kind FROM change_batch_mutations
             WHERE batch_id = ?1 AND mutation_id = ?2",
            params![batch_id.to_string(), mutation_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some_and(|kind| kind == "legacy_v3"))
}

fn transfer_backed_mutation_count(connection: &Connection, batch_id: Uuid) -> Result<u64> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*)
         FROM change_batch_mutations AS mutation
         JOIN transfers AS transfer ON transfer.operation_id = mutation.mutation_id
         WHERE mutation.batch_id = ?1",
        [batch_id.to_string()],
        |row| row.get(0),
    )?;
    u64::try_from(count).map_err(|_| Error::InvalidSchema)
}

fn legacy_v3_dependency_count(connection: &Connection, batch_id: Uuid) -> Result<u64> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM change_batch_mutations
         WHERE batch_id = ?1 AND dependency_kind = 'legacy_v3'",
        [batch_id.to_string()],
        |row| row.get(0),
    )?;
    u64::try_from(count).map_err(|_| Error::InvalidSchema)
}

fn typed_r3_dependency_count(connection: &Connection, batch_id: Uuid) -> Result<u64> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM change_batch_mutations
         WHERE batch_id = ?1 AND dependency_kind != 'legacy_v3'",
        [batch_id.to_string()],
        |row| row.get(0),
    )?;
    u64::try_from(count).map_err(|_| Error::InvalidSchema)
}

fn require_exact_r3_completion_evidence(
    connection: &Connection,
    operation_id: Uuid,
    evidence_id: Uuid,
) -> Result<()> {
    let state = load_mutation_state(connection, operation_id)?.ok_or(Error::MutationNotFound)?;
    if state.phase == MutationPhase::NeedsReconcile {
        return Err(Error::MutationNeedsReconcile);
    }
    if state.phase != MutationPhase::Completed
        || state.disposition != Some(MutationDisposition::VerifiedApplied)
        || state.last_evidence_id != Some(evidence_id)
    {
        return Err(Error::LocalMutationIncomplete);
    }
    let evidence = connection
        .query_row(
            "SELECT capture_phase, disposition, forbidden_side_effect, attempt_number, outcome_code
             FROM mutation_verification_evidence
             WHERE evidence_id = ?1 AND operation_id = ?2",
            params![evidence_id.to_string(), operation_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or(Error::LocalMutationIncomplete)?;
    if evidence.0 != "post_verify"
        || evidence.1 != "verified_applied"
        || evidence.2 != 0
        || u32::try_from(evidence.3).ok() != Some(state.attempt_number)
        || evidence.4 != state.outcome_code
    {
        return Err(Error::LocalMutationIncomplete);
    }
    let event_exists = connection
        .query_row(
            "SELECT 1 FROM mutation_events
             WHERE operation_id = ?1 AND evidence_id = ?2
               AND attempt_number = ?3 AND state_version = ?4
               AND phase = 'completed' AND disposition = 'verified_applied'
               AND outcome_code IS ?5",
            params![
                operation_id.to_string(),
                evidence_id.to_string(),
                i64::from(state.attempt_number),
                u64_to_i64(state.state_version)?,
                state.outcome_code,
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !event_exists {
        return Err(Error::LocalMutationIncomplete);
    }
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

fn insert_transfer(transaction: &Transaction<'_>, transfer: &TransferRecord) -> Result<()> {
    transaction.execute(
        "INSERT INTO transfers(
            operation_id, direction, portable_path, remote_parent_id, remote_file_id,
            display_name, expected_local_revision, expected_remote_revision, sha256,
            byte_length, mime_class, operation_marker, stage_reference, base_reference,
            phase, attempt_count, next_attempt_at_unix_ms, created_at_unix_ms,
            updated_at_unix_ms, last_error_code, verified_local_revision,
            verified_remote_revision
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
            ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22
         )",
        params![
            transfer.operation_id.to_string(),
            transfer.direction.as_str(),
            transfer.portable_path,
            transfer.remote_parent_id,
            transfer.remote_file_id,
            transfer.display_name,
            transfer.expected_local_revision,
            transfer.expected_remote_revision,
            transfer.sha256,
            u64_to_i64(transfer.byte_length)?,
            transfer.mime_class.as_str(),
            transfer.operation_marker,
            transfer.stage_reference,
            transfer.base_reference,
            transfer.phase.as_str(),
            i64::from(transfer.attempt_count),
            u64_to_i64(transfer.next_attempt_at_unix_ms)?,
            u64_to_i64(transfer.created_at_unix_ms)?,
            u64_to_i64(transfer.updated_at_unix_ms)?,
            transfer.last_error_code,
            transfer.verified_local_revision,
            transfer.verified_remote_revision,
        ],
    )?;
    Ok(())
}

fn load_transfer(connection: &Connection, operation_id: Uuid) -> Result<Option<TransferRecord>> {
    connection
        .query_row(
            "SELECT operation_id, direction, portable_path, remote_parent_id,
                    remote_file_id, display_name, expected_local_revision,
                    expected_remote_revision, sha256, byte_length, mime_class,
                    operation_marker, stage_reference, base_reference, phase,
                    attempt_count, next_attempt_at_unix_ms, created_at_unix_ms,
                    updated_at_unix_ms, last_error_code, verified_local_revision,
                    verified_remote_revision
             FROM transfers WHERE operation_id = ?1",
            [operation_id.to_string()],
            row_to_transfer,
        )
        .optional()?
        .map_or(Ok(None), |transfer| Ok(Some(transfer?)))
}

fn row_to_transfer(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<TransferRecord>> {
    let operation_id = row.get::<_, String>(0)?;
    let direction = row.get::<_, String>(1)?;
    let portable_path = row.get::<_, String>(2)?;
    let remote_parent_id = row.get::<_, String>(3)?;
    let remote_file_id = row.get::<_, Option<String>>(4)?;
    let display_name = row.get::<_, String>(5)?;
    let expected_local_revision = row.get::<_, Option<String>>(6)?;
    let expected_remote_revision = row.get::<_, Option<String>>(7)?;
    let sha256 = row.get::<_, String>(8)?;
    let byte_length = row.get::<_, i64>(9)?;
    let mime_class = row.get::<_, String>(10)?;
    let operation_marker = row.get::<_, String>(11)?;
    let stage_reference = row.get::<_, Option<String>>(12)?;
    let base_reference = row.get::<_, Option<String>>(13)?;
    let phase = row.get::<_, String>(14)?;
    let attempt_count = row.get::<_, i64>(15)?;
    let next_attempt_at_unix_ms = row.get::<_, i64>(16)?;
    let created_at_unix_ms = row.get::<_, i64>(17)?;
    let updated_at_unix_ms = row.get::<_, i64>(18)?;
    let last_error_code = row.get::<_, Option<String>>(19)?;
    let verified_local_revision = row.get::<_, Option<String>>(20)?;
    let verified_remote_revision = row.get::<_, Option<String>>(21)?;
    Ok((|| {
        let transfer = TransferRecord {
            operation_id: parse_uuid(&operation_id)?,
            direction: TransferDirection::parse(&direction)?,
            portable_path,
            remote_parent_id,
            remote_file_id,
            display_name,
            expected_local_revision,
            expected_remote_revision,
            sha256,
            byte_length: u64::try_from(byte_length).map_err(|_| Error::InvalidSchema)?,
            mime_class: TransferMimeClass::parse(&mime_class)?,
            operation_marker,
            stage_reference,
            base_reference,
            phase: TransferPhase::parse(&phase)?,
            attempt_count: u32::try_from(attempt_count).map_err(|_| Error::InvalidSchema)?,
            next_attempt_at_unix_ms: u64::try_from(next_attempt_at_unix_ms)
                .map_err(|_| Error::InvalidSchema)?,
            created_at_unix_ms: u64::try_from(created_at_unix_ms)
                .map_err(|_| Error::InvalidSchema)?,
            updated_at_unix_ms: u64::try_from(updated_at_unix_ms)
                .map_err(|_| Error::InvalidSchema)?,
            last_error_code,
            verified_local_revision,
            verified_remote_revision,
        };
        transfer.validate()?;
        Ok(transfer)
    })())
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
