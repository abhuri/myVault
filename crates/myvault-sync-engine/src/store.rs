use crate::local_orchestration::{authoritative_outcome_id, AuthoritativeFinalOutcome};
use crate::sync_journal::{
    BridgeConsumptionWitness, OutcomeWitness, PreSideEffectWitness, SyncExecutionJournal,
};
use crate::{
    local_identity::{
        local_intent_fingerprint_from_r3_intent, persisted_canonical_collision_key,
        persisted_stable_identity_fingerprint, DurableExecutionBinding,
        DurableExecutionBindingFingerprint, PersistedIdentityEvidence,
    },
    parse_uuid, u64_to_i64, validate_content_path, validate_private_reference,
    validate_redacted_code, validate_remote_id, validate_remote_token, validate_revision,
    ChangesPage, Error, RemoteChange, RemoteContentHash, RemoteEntry, RemoteEntryKind,
    RemoteHashAlgorithm, Result, ScanPage, ScanRequest, SyncPhase, VerifiedRemoteBinding,
    MAX_SCAN_FRONTIER_FOLDERS,
};
#[cfg(target_os = "android")]
use cap_fs_ext::OpenOptionsMaybeDirExt;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use fs2::FileExt;
use myvault_private_fs as private_fs;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) const ROOT_DIRECTORY: &str = "sync-state";
pub(crate) const VERSION_DIRECTORY: &str = "v1";
pub(crate) const VAULTS_DIRECTORY: &str = "vaults";
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
type LocalExecutionContractFingerprints = (Uuid, [u8; 32], [u8; 32], [u8; 32]);
type LocalBridgeSnapshot = (
    String,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    String,
    i64,
    String,
    String,
    String,
    Vec<u8>,
    i64,
    String,
    i64,
    String,
    String,
    i64,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);
type R3_5CursorProofRow = (
    String,
    Vec<u8>,
    String,
    i64,
    String,
    i64,
    Vec<u8>,
    String,
    String,
    Vec<u8>,
    Vec<u8>,
    i64,
    String,
    String,
    String,
    i64,
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
);
type SemanticIdentityRow = (i64, i64, i64, Vec<u8>, Vec<u8>, Vec<u8>);
#[derive(Clone)]
struct BridgeReceiptFacts {
    operation_id: Uuid,
    attempt_number: u32,
    boundary_id: Uuid,
    boundary_occurred_at_unix_ms: u64,
    contract_fingerprint: [u8; 32],
    outcome_id: Uuid,
    evidence_id: Uuid,
    local_evidence_fingerprint: [u8; 32],
    outcome_occurred_at_unix_ms: u64,
    r3_intent_fingerprint: String,
    r3_evidence_fingerprint: String,
    // `NULL` is a real R3 fact, not an omitted field.  It is therefore bound
    // with a presence byte in the receipt preimage below.
    r3_outcome_code: Option<String>,
    dependency_kind: String,
    r3_state_phase: String,
    r3_state_disposition: String,
    r3_attempt_number: u32,
    r3_state_version: u64,
    r3_last_evidence_id: Uuid,
    r3_event_state_version: u64,
}

#[derive(Clone, Copy)]
enum JournalR3Expectation {
    /// An ordinary local finalization has no R3 authority or R3 fingerprint.
    GenericUnbridged,
    /// An authoritative finalization published its R3 proof, but crashed
    /// before the receipt transaction.  This is recoverable only while no
    /// committed dependency exists.
    AuthoritativePreReceipt([u8; 32]),
    /// The receipt seals this exact R3 evidence fingerprint.
    Bridged([u8; 32]),
}

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

const LOCAL_EXECUTION_CONTRACTS_SCHEMA: &str = "CREATE TABLE local_execution_contracts (
    operation_id TEXT PRIMARY KEY NOT NULL,
    vault_id TEXT NOT NULL,
    intent_fingerprint BLOB NOT NULL CHECK (typeof(intent_fingerprint) = 'blob' AND length(intent_fingerprint) = 32),
    contract_fingerprint BLOB NOT NULL UNIQUE CHECK (typeof(contract_fingerprint) = 'blob' AND length(contract_fingerprint) = 32),
    target_name TEXT NOT NULL CHECK (typeof(target_name) = 'text' AND length(CAST(target_name AS BLOB)) BETWEEN 1 AND 255),
    target_collision_key TEXT NOT NULL CHECK (typeof(target_collision_key) = 'text' AND length(CAST(target_collision_key AS BLOB)) BETWEEN 1 AND 1024),
    collision_member_count INTEGER NOT NULL CHECK (collision_member_count BETWEEN 0 AND 4096),
    collision_snapshot_fingerprint BLOB NOT NULL CHECK (typeof(collision_snapshot_fingerprint) = 'blob' AND length(collision_snapshot_fingerprint) = 32),
    completion_id TEXT NOT NULL UNIQUE,
    registered_at_unix_ms INTEGER NOT NULL CHECK (registered_at_unix_ms >= 0),
    FOREIGN KEY (completion_id) REFERENCES local_execution_contract_completions(completion_id) DEFERRABLE INITIALLY DEFERRED
)";
const LOCAL_EXECUTION_IDENTITY_EVIDENCE_SCHEMA: &str = "CREATE TABLE local_execution_identity_evidence (
    evidence_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('vault_root', 'source_parent', 'source_object', 'destination_parent', 'collision_parent_start', 'collision_parent_end')),
    evidence_version INTEGER NOT NULL CHECK (evidence_version = 1),
    evidence_kind INTEGER NOT NULL CHECK (evidence_kind = 1),
    object_kind INTEGER NOT NULL CHECK (object_kind IN (1, 2)),
    provider_id BLOB NOT NULL CHECK (typeof(provider_id) = 'blob' AND length(provider_id) BETWEEN 1 AND 128),
    object_id BLOB NOT NULL CHECK (typeof(object_id) = 'blob' AND length(object_id) BETWEEN 1 AND 1024),
    attestation BLOB NOT NULL CHECK (typeof(attestation) = 'blob' AND length(attestation) BETWEEN 1 AND 1024),
    stable_identity_fingerprint BLOB NOT NULL CHECK (typeof(stable_identity_fingerprint) = 'blob' AND length(stable_identity_fingerprint) = 32),
    UNIQUE (operation_id, role),
    CHECK (role = 'source_object' OR object_kind = 1),
    FOREIGN KEY (operation_id) REFERENCES local_execution_contracts(operation_id) DEFERRABLE INITIALLY DEFERRED
)";
const LOCAL_EXECUTION_COLLISION_MEMBERS_SCHEMA: &str = "CREATE TABLE local_execution_collision_members (
    operation_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    name TEXT NOT NULL CHECK (typeof(name) = 'text' AND length(CAST(name AS BLOB)) BETWEEN 1 AND 255),
    collision_key TEXT NOT NULL CHECK (typeof(collision_key) = 'text' AND length(CAST(collision_key AS BLOB)) BETWEEN 1 AND 1024),
    evidence_version INTEGER NOT NULL CHECK (evidence_version = 1),
    evidence_kind INTEGER NOT NULL CHECK (evidence_kind = 1),
    object_kind INTEGER NOT NULL CHECK (object_kind IN (1, 2)),
    provider_id BLOB NOT NULL CHECK (typeof(provider_id) = 'blob' AND length(provider_id) BETWEEN 1 AND 128),
    object_id BLOB NOT NULL CHECK (typeof(object_id) = 'blob' AND length(object_id) BETWEEN 1 AND 1024),
    attestation BLOB NOT NULL CHECK (typeof(attestation) = 'blob' AND length(attestation) BETWEEN 1 AND 1024),
    stable_identity_fingerprint BLOB NOT NULL CHECK (typeof(stable_identity_fingerprint) = 'blob' AND length(stable_identity_fingerprint) = 32),
    PRIMARY KEY (operation_id, ordinal),
    UNIQUE (operation_id, name),
    UNIQUE (operation_id, stable_identity_fingerprint),
    FOREIGN KEY (operation_id) REFERENCES local_execution_contracts(operation_id) DEFERRABLE INITIALLY DEFERRED
)";
const LOCAL_EXECUTION_CONTRACT_COMPLETIONS_SCHEMA: &str = "CREATE TABLE local_execution_contract_completions (
    completion_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    completed_at_unix_ms INTEGER NOT NULL CHECK (completed_at_unix_ms >= 0),
    FOREIGN KEY (operation_id) REFERENCES local_execution_contracts(operation_id) DEFERRABLE INITIALLY DEFERRED
)";
const LOCAL_EXECUTION_ATTEMPT_BOUNDARIES_SCHEMA: &str =
    "CREATE TABLE local_execution_attempt_boundaries (
    operation_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL CHECK (attempt_number >= 0),
    boundary_id TEXT NOT NULL UNIQUE,
    contract_fingerprint BLOB NOT NULL CHECK (typeof(contract_fingerprint) = 'blob' AND length(contract_fingerprint) = 32),
    occurred_at_unix_ms INTEGER NOT NULL CHECK (occurred_at_unix_ms >= 0),
    PRIMARY KEY (operation_id, attempt_number),
    FOREIGN KEY (operation_id) REFERENCES local_execution_contracts(operation_id)
)";
const LOCAL_EXECUTION_ATTEMPT_OUTCOMES_SCHEMA: &str = "CREATE TABLE local_execution_attempt_outcomes (
    operation_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL CHECK (attempt_number >= 0),
    outcome_id TEXT NOT NULL UNIQUE,
    evidence_id TEXT NOT NULL UNIQUE,
    outcome TEXT NOT NULL CHECK (outcome IN ('VerifiedApplied', 'VerifiedNotApplied', 'WriteOutcomeUnknown', 'NeedsReconcile')),
    contract_fingerprint BLOB NOT NULL CHECK (typeof(contract_fingerprint) = 'blob' AND length(contract_fingerprint) = 32),
    evidence_fingerprint BLOB NOT NULL CHECK (typeof(evidence_fingerprint) = 'blob' AND length(evidence_fingerprint) = 32),
    non_retryable INTEGER NOT NULL CHECK (non_retryable IN (0, 1)),
    occurred_at_unix_ms INTEGER NOT NULL CHECK (occurred_at_unix_ms >= 0),
    PRIMARY KEY (operation_id, attempt_number),
    FOREIGN KEY (operation_id, attempt_number) REFERENCES local_execution_attempt_boundaries(operation_id, attempt_number),
    CHECK ((outcome IN ('WriteOutcomeUnknown', 'NeedsReconcile') AND non_retryable = 1) OR (outcome IN ('VerifiedApplied', 'VerifiedNotApplied') AND non_retryable = 0))
)";
// Kept in schema version 6 deliberately: v6 has not shipped and this is an
// additive local-only execution proof, not a new protocol generation.
const LOCAL_EXECUTION_R3_BRIDGE_RECEIPTS_SCHEMA: &str = "CREATE TABLE local_execution_r3_bridge_receipts (
    receipt_id TEXT PRIMARY KEY NOT NULL,
    receipt_fingerprint BLOB NOT NULL UNIQUE CHECK (typeof(receipt_fingerprint) = 'blob' AND length(receipt_fingerprint) = 32),
    operation_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL CHECK (attempt_number BETWEEN 0 AND 4294967295),
    boundary_id TEXT NOT NULL,
    boundary_occurred_at_unix_ms INTEGER NOT NULL CHECK (boundary_occurred_at_unix_ms >= 0),
    contract_fingerprint BLOB NOT NULL CHECK (typeof(contract_fingerprint) = 'blob' AND length(contract_fingerprint) = 32),
    outcome_id TEXT NOT NULL,
    evidence_id TEXT NOT NULL,
    local_evidence_fingerprint BLOB NOT NULL CHECK (typeof(local_evidence_fingerprint) = 'blob' AND length(local_evidence_fingerprint) = 32),
    outcome_occurred_at_unix_ms INTEGER NOT NULL CHECK (outcome_occurred_at_unix_ms >= 0),
    r3_intent_fingerprint TEXT NOT NULL,
    r3_evidence_fingerprint TEXT NOT NULL,
    r3_outcome_code TEXT,
    dependency_kind TEXT NOT NULL CHECK (dependency_kind IN ('mutation', 'merge_publication', 'conflict_copy_publication', 'base_publication')),
    r3_state_phase TEXT NOT NULL CHECK (r3_state_phase = 'completed'),
    r3_state_disposition TEXT NOT NULL CHECK (r3_state_disposition = 'verified_applied'),
    r3_attempt_number INTEGER NOT NULL CHECK (r3_attempt_number BETWEEN 0 AND 4294967295),
    r3_state_version INTEGER NOT NULL CHECK (r3_state_version >= 0),
    r3_last_evidence_id TEXT NOT NULL,
    r3_event_state_version INTEGER NOT NULL CHECK (r3_event_state_version >= 0),
    FOREIGN KEY (operation_id, attempt_number) REFERENCES local_execution_attempt_boundaries(operation_id, attempt_number),
    FOREIGN KEY (operation_id, attempt_number) REFERENCES local_execution_attempt_outcomes(operation_id, attempt_number),
    FOREIGN KEY (evidence_id) REFERENCES mutation_verification_evidence(evidence_id),
    UNIQUE (operation_id, attempt_number),
    CHECK (r3_last_evidence_id = evidence_id)
)";
// Retained independently from the live receipt and batch rows.  This is a
// consumption audit fact, never a cascade child: cleanup of a completed batch
// must not make a consumed bridge look like a pre-receipt crash.
const LOCAL_EXECUTION_R3_CONSUMPTION_ANCHORS_SCHEMA: &str = "CREATE TABLE local_execution_r3_consumption_anchors (
    anchor_id TEXT PRIMARY KEY NOT NULL,
    anchor_fingerprint BLOB NOT NULL UNIQUE CHECK (typeof(anchor_fingerprint) = 'blob' AND length(anchor_fingerprint) = 32),
    receipt_id TEXT NOT NULL UNIQUE,
    receipt_fingerprint BLOB NOT NULL UNIQUE CHECK (typeof(receipt_fingerprint) = 'blob' AND length(receipt_fingerprint) = 32),
    operation_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL CHECK (attempt_number BETWEEN 0 AND 4294967295),
    outcome_id TEXT NOT NULL,
    evidence_id TEXT NOT NULL,
    r3_evidence_fingerprint TEXT NOT NULL,
    dependency_kind TEXT NOT NULL CHECK (dependency_kind IN ('mutation', 'merge_publication', 'conflict_copy_publication', 'base_publication')),
    UNIQUE (operation_id, attempt_number)
)";
// A RetryScheduled event loses its state fields after claim.  Retain the
// complete proof as an immutable event companion so reopening can prove the
// exact contract rather than merely observing a coherent current state.
const MUTATION_RETRY_CONTRACTS_SCHEMA: &str = "CREATE TABLE mutation_retry_contracts (
    operation_id TEXT NOT NULL,
    state_version INTEGER NOT NULL CHECK (state_version >= 0),
    attempt_number INTEGER NOT NULL CHECK (attempt_number BETWEEN 0 AND 4294967295),
    evidence_id TEXT NOT NULL UNIQUE,
    evidence_fingerprint TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK (disposition IN ('verified_not_applied', 'retry_safe')),
    outcome_code TEXT NOT NULL,
    due_at_unix_ms INTEGER NOT NULL CHECK (due_at_unix_ms >= 0),
    retry_mode TEXT NOT NULL CHECK (retry_mode IN ('restart_exact', 'resume_exact')),
    resume_reference TEXT,
    verified_received_byte_offset INTEGER,
    captured_at_unix_ms INTEGER NOT NULL CHECK (captured_at_unix_ms >= 0),
    PRIMARY KEY (operation_id, state_version),
    UNIQUE (operation_id, attempt_number),
    CHECK ((disposition = 'verified_not_applied' AND outcome_code = 'verified_not_applied' AND retry_mode = 'restart_exact' AND resume_reference IS NULL AND verified_received_byte_offset IS NULL) OR
           (disposition = 'retry_safe' AND outcome_code = 'retry_safe' AND retry_mode = 'resume_exact' AND resume_reference IS NOT NULL AND verified_received_byte_offset IS NOT NULL AND verified_received_byte_offset >= 0))
)";
const LOCAL_EXECUTION_CONTRACTS_VAULT_INDEX_SCHEMA: &str =
    "CREATE INDEX local_execution_contracts_vault_idx
    ON local_execution_contracts(vault_id, registered_at_unix_ms, operation_id)";
const LOCAL_EXECUTION_IDENTITY_OPERATION_INDEX_SCHEMA: &str =
    "CREATE INDEX local_execution_identity_operation_idx
    ON local_execution_identity_evidence(operation_id, role)";
const LOCAL_EXECUTION_BOUNDARY_CONTRACT_INDEX_SCHEMA: &str =
    "CREATE INDEX local_execution_boundary_contract_idx
    ON local_execution_attempt_boundaries(operation_id, contract_fingerprint, attempt_number)";
const LOCAL_EXECUTION_BRIDGE_RECEIPT_OPERATION_INDEX_SCHEMA: &str =
    "CREATE INDEX local_execution_bridge_receipt_operation_idx
    ON local_execution_r3_bridge_receipts(operation_id, evidence_id, dependency_kind)";
const LOCAL_EXECUTION_CONSUMPTION_ANCHOR_OPERATION_INDEX_SCHEMA: &str =
    "CREATE INDEX local_execution_consumption_anchor_operation_idx
    ON local_execution_r3_consumption_anchors(operation_id, evidence_id, dependency_kind)";
const MUTATION_RETRY_CONTRACT_OPERATION_INDEX_SCHEMA: &str =
    "CREATE INDEX mutation_retry_contract_operation_idx
    ON mutation_retry_contracts(operation_id, attempt_number, state_version)";
const LOCAL_EXECUTION_COMPLETION_VALIDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_completion_validate
    BEFORE INSERT ON local_execution_contract_completions BEGIN
      SELECT CASE WHEN NOT EXISTS (SELECT 1 FROM local_execution_contracts WHERE operation_id = NEW.operation_id)
        OR (SELECT COUNT(*) FROM local_execution_identity_evidence WHERE operation_id = NEW.operation_id) != 6
        OR (SELECT COUNT(*) FROM local_execution_identity_evidence WHERE operation_id = NEW.operation_id AND role IN ('vault_root', 'source_parent', 'source_object', 'destination_parent', 'collision_parent_start', 'collision_parent_end')) != 6
        OR (SELECT stable_identity_fingerprint FROM local_execution_identity_evidence WHERE operation_id = NEW.operation_id AND role = 'destination_parent') != (SELECT stable_identity_fingerprint FROM local_execution_identity_evidence WHERE operation_id = NEW.operation_id AND role = 'collision_parent_start')
        OR (SELECT stable_identity_fingerprint FROM local_execution_identity_evidence WHERE operation_id = NEW.operation_id AND role = 'destination_parent') != (SELECT stable_identity_fingerprint FROM local_execution_identity_evidence WHERE operation_id = NEW.operation_id AND role = 'collision_parent_end')
        OR (SELECT COUNT(*) FROM local_execution_collision_members WHERE operation_id = NEW.operation_id) != (SELECT collision_member_count FROM local_execution_contracts WHERE operation_id = NEW.operation_id)
        OR EXISTS (SELECT 1 FROM local_execution_collision_members AS member JOIN local_execution_contracts AS contract ON contract.operation_id = member.operation_id WHERE member.operation_id = NEW.operation_id AND member.ordinal >= contract.collision_member_count)
        THEN RAISE(ABORT, 'local_execution_contract_incomplete') END;
    END";
const LOCAL_EXECUTION_MEMBER_RANGE_TRIGGER: &str = "CREATE TRIGGER local_execution_member_range
    BEFORE INSERT ON local_execution_collision_members BEGIN
      SELECT CASE WHEN NOT EXISTS (SELECT 1 FROM local_execution_contracts WHERE operation_id = NEW.operation_id AND NEW.ordinal < collision_member_count)
        THEN RAISE(ABORT, 'local_execution_collision_member_out_of_range') END;
    END";
const LOCAL_EXECUTION_BOUNDARY_VALIDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_boundary_validate
    BEFORE INSERT ON local_execution_attempt_boundaries BEGIN
      SELECT CASE WHEN NOT EXISTS (SELECT 1 FROM local_execution_contracts WHERE operation_id = NEW.operation_id AND contract_fingerprint = NEW.contract_fingerprint)
        THEN RAISE(ABORT, 'local_execution_boundary_contract_mismatch') END;
    END";
const LOCAL_EXECUTION_OUTCOME_VALIDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_outcome_validate
    BEFORE INSERT ON local_execution_attempt_outcomes BEGIN
      SELECT CASE WHEN NOT EXISTS (SELECT 1 FROM local_execution_contracts WHERE operation_id = NEW.operation_id AND contract_fingerprint = NEW.contract_fingerprint)
        THEN RAISE(ABORT, 'local_execution_outcome_contract_mismatch') END;
    END";
const LOCAL_EXECUTION_CONTRACTS_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_contracts_no_update
    BEFORE UPDATE ON local_execution_contracts BEGIN SELECT RAISE(ABORT, 'local_execution_contracts_immutable'); END";
const LOCAL_EXECUTION_CONTRACTS_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER local_execution_contracts_no_delete
    BEFORE DELETE ON local_execution_contracts BEGIN SELECT RAISE(ABORT, 'local_execution_contracts_immutable'); END";
const LOCAL_EXECUTION_IDENTITIES_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_identities_no_update
    BEFORE UPDATE ON local_execution_identity_evidence BEGIN SELECT RAISE(ABORT, 'local_execution_identity_evidence_immutable'); END";
const LOCAL_EXECUTION_IDENTITIES_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER local_execution_identities_no_delete
    BEFORE DELETE ON local_execution_identity_evidence BEGIN SELECT RAISE(ABORT, 'local_execution_identity_evidence_immutable'); END";
const LOCAL_EXECUTION_MEMBERS_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_members_no_update
    BEFORE UPDATE ON local_execution_collision_members BEGIN SELECT RAISE(ABORT, 'local_execution_collision_members_immutable'); END";
const LOCAL_EXECUTION_MEMBERS_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER local_execution_members_no_delete
    BEFORE DELETE ON local_execution_collision_members BEGIN SELECT RAISE(ABORT, 'local_execution_collision_members_immutable'); END";
const LOCAL_EXECUTION_COMPLETIONS_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_completions_no_update
    BEFORE UPDATE ON local_execution_contract_completions BEGIN SELECT RAISE(ABORT, 'local_execution_contract_completions_immutable'); END";
const LOCAL_EXECUTION_COMPLETIONS_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER local_execution_completions_no_delete
    BEFORE DELETE ON local_execution_contract_completions BEGIN SELECT RAISE(ABORT, 'local_execution_contract_completions_immutable'); END";
const LOCAL_EXECUTION_BOUNDARIES_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_boundaries_no_update
    BEFORE UPDATE ON local_execution_attempt_boundaries BEGIN SELECT RAISE(ABORT, 'local_execution_attempt_boundaries_immutable'); END";
const LOCAL_EXECUTION_BOUNDARIES_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER local_execution_boundaries_no_delete
    BEFORE DELETE ON local_execution_attempt_boundaries BEGIN SELECT RAISE(ABORT, 'local_execution_attempt_boundaries_immutable'); END";
const LOCAL_EXECUTION_OUTCOMES_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_outcomes_no_update
    BEFORE UPDATE ON local_execution_attempt_outcomes BEGIN SELECT RAISE(ABORT, 'local_execution_attempt_outcomes_immutable'); END";
const LOCAL_EXECUTION_OUTCOMES_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER local_execution_outcomes_no_delete
    BEFORE DELETE ON local_execution_attempt_outcomes BEGIN SELECT RAISE(ABORT, 'local_execution_attempt_outcomes_immutable'); END";
const LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_bridge_receipts_no_update
    BEFORE UPDATE ON local_execution_r3_bridge_receipts BEGIN SELECT RAISE(ABORT, 'local_execution_r3_bridge_receipts_immutable'); END";
const LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER local_execution_bridge_receipts_no_delete
    BEFORE DELETE ON local_execution_r3_bridge_receipts BEGIN SELECT RAISE(ABORT, 'local_execution_r3_bridge_receipts_immutable'); END";
const LOCAL_EXECUTION_CONSUMPTION_ANCHORS_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER local_execution_consumption_anchors_no_update
    BEFORE UPDATE ON local_execution_r3_consumption_anchors BEGIN SELECT RAISE(ABORT, 'local_execution_r3_consumption_anchors_immutable'); END";
const LOCAL_EXECUTION_CONSUMPTION_ANCHORS_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER local_execution_consumption_anchors_no_delete
    BEFORE DELETE ON local_execution_r3_consumption_anchors BEGIN SELECT RAISE(ABORT, 'local_execution_r3_consumption_anchors_immutable'); END";
const MUTATION_RETRY_CONTRACTS_NO_UPDATE_TRIGGER: &str = "CREATE TRIGGER mutation_retry_contracts_no_update
    BEFORE UPDATE ON mutation_retry_contracts BEGIN SELECT RAISE(ABORT, 'mutation_retry_contracts_immutable'); END";
const MUTATION_RETRY_CONTRACTS_NO_DELETE_TRIGGER: &str = "CREATE TRIGGER mutation_retry_contracts_no_delete
    BEFORE DELETE ON mutation_retry_contracts BEGIN SELECT RAISE(ABORT, 'mutation_retry_contracts_immutable'); END";

const LOCAL_EXECUTION_SCHEMA_OBJECTS: [(&str, &str, &str); 37] = [
    (
        "table",
        "local_execution_contracts",
        LOCAL_EXECUTION_CONTRACTS_SCHEMA,
    ),
    (
        "table",
        "local_execution_identity_evidence",
        LOCAL_EXECUTION_IDENTITY_EVIDENCE_SCHEMA,
    ),
    (
        "table",
        "local_execution_collision_members",
        LOCAL_EXECUTION_COLLISION_MEMBERS_SCHEMA,
    ),
    (
        "table",
        "local_execution_contract_completions",
        LOCAL_EXECUTION_CONTRACT_COMPLETIONS_SCHEMA,
    ),
    (
        "table",
        "local_execution_attempt_boundaries",
        LOCAL_EXECUTION_ATTEMPT_BOUNDARIES_SCHEMA,
    ),
    (
        "table",
        "local_execution_attempt_outcomes",
        LOCAL_EXECUTION_ATTEMPT_OUTCOMES_SCHEMA,
    ),
    (
        "table",
        "local_execution_r3_bridge_receipts",
        LOCAL_EXECUTION_R3_BRIDGE_RECEIPTS_SCHEMA,
    ),
    (
        "table",
        "local_execution_r3_consumption_anchors",
        LOCAL_EXECUTION_R3_CONSUMPTION_ANCHORS_SCHEMA,
    ),
    (
        "table",
        "mutation_retry_contracts",
        MUTATION_RETRY_CONTRACTS_SCHEMA,
    ),
    (
        "index",
        "local_execution_contracts_vault_idx",
        LOCAL_EXECUTION_CONTRACTS_VAULT_INDEX_SCHEMA,
    ),
    (
        "index",
        "local_execution_identity_operation_idx",
        LOCAL_EXECUTION_IDENTITY_OPERATION_INDEX_SCHEMA,
    ),
    (
        "index",
        "local_execution_boundary_contract_idx",
        LOCAL_EXECUTION_BOUNDARY_CONTRACT_INDEX_SCHEMA,
    ),
    (
        "index",
        "local_execution_bridge_receipt_operation_idx",
        LOCAL_EXECUTION_BRIDGE_RECEIPT_OPERATION_INDEX_SCHEMA,
    ),
    (
        "index",
        "local_execution_consumption_anchor_operation_idx",
        LOCAL_EXECUTION_CONSUMPTION_ANCHOR_OPERATION_INDEX_SCHEMA,
    ),
    (
        "index",
        "mutation_retry_contract_operation_idx",
        MUTATION_RETRY_CONTRACT_OPERATION_INDEX_SCHEMA,
    ),
    (
        "trigger",
        "local_execution_completion_validate",
        LOCAL_EXECUTION_COMPLETION_VALIDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_member_range",
        LOCAL_EXECUTION_MEMBER_RANGE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_boundary_validate",
        LOCAL_EXECUTION_BOUNDARY_VALIDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_outcome_validate",
        LOCAL_EXECUTION_OUTCOME_VALIDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_contracts_no_update",
        LOCAL_EXECUTION_CONTRACTS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_contracts_no_delete",
        LOCAL_EXECUTION_CONTRACTS_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_identities_no_update",
        LOCAL_EXECUTION_IDENTITIES_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_identities_no_delete",
        LOCAL_EXECUTION_IDENTITIES_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_members_no_update",
        LOCAL_EXECUTION_MEMBERS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_members_no_delete",
        LOCAL_EXECUTION_MEMBERS_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_completions_no_update",
        LOCAL_EXECUTION_COMPLETIONS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_completions_no_delete",
        LOCAL_EXECUTION_COMPLETIONS_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_boundaries_no_update",
        LOCAL_EXECUTION_BOUNDARIES_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_boundaries_no_delete",
        LOCAL_EXECUTION_BOUNDARIES_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_outcomes_no_update",
        LOCAL_EXECUTION_OUTCOMES_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_outcomes_no_delete",
        LOCAL_EXECUTION_OUTCOMES_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_bridge_receipts_no_update",
        LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_bridge_receipts_no_delete",
        LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_consumption_anchors_no_update",
        LOCAL_EXECUTION_CONSUMPTION_ANCHORS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "local_execution_consumption_anchors_no_delete",
        LOCAL_EXECUTION_CONSUMPTION_ANCHORS_NO_DELETE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_retry_contracts_no_update",
        MUTATION_RETRY_CONTRACTS_NO_UPDATE_TRIGGER,
    ),
    (
        "trigger",
        "mutation_retry_contracts_no_delete",
        MUTATION_RETRY_CONTRACTS_NO_DELETE_TRIGGER,
    ),
];

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

const SCHEMA_OBJECTS_V5: [(&str, &str, &str); 31] = [
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

pub const SCHEMA_VERSION: i64 = 6;
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

/// Result of atomically registering immutable local execution evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalExecutionRegistrationOutcome {
    Registered,
    AlreadyPresent,
}

/// The only vocabulary stored for local execution outcomes.
///
/// These append-only `SQLite` ledger rows are evidence only. They neither
/// authorize filesystem execution nor schedule a retry or recovery action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalExecutionOutcome {
    VerifiedApplied,
    VerifiedNotApplied,
    WriteOutcomeUnknown,
    NeedsReconcile,
}

impl LocalExecutionOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::VerifiedApplied => "VerifiedApplied",
            Self::VerifiedNotApplied => "VerifiedNotApplied",
            Self::WriteOutcomeUnknown => "WriteOutcomeUnknown",
            Self::NeedsReconcile => "NeedsReconcile",
        }
    }

    const fn non_retryable(self) -> bool {
        matches!(self, Self::WriteOutcomeUnknown | Self::NeedsReconcile)
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "VerifiedApplied" => Ok(Self::VerifiedApplied),
            "VerifiedNotApplied" => Ok(Self::VerifiedNotApplied),
            "WriteOutcomeUnknown" => Ok(Self::WriteOutcomeUnknown),
            "NeedsReconcile" => Ok(Self::NeedsReconcile),
            _ => Err(Error::InvalidSchema),
        }
    }
}

/// Input for one append-only local execution attempt boundary.
///
/// The binding fingerprint must be the same immutable contract registered for
/// `operation_id`; the ledger does not grant execution authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalExecutionAttemptBoundary {
    pub operation_id: Uuid,
    pub attempt_number: u32,
    pub boundary_id: Uuid,
    pub contract_fingerprint: DurableExecutionBindingFingerprint,
    pub occurred_at_unix_ms: u64,
}

/// Input for one append-only local execution outcome.
///
/// `WriteOutcomeUnknown` is always stored as non-retryable. This API never
/// fabricates an outcome and does not schedule execution or reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalExecutionAttemptOutcome {
    pub operation_id: Uuid,
    pub attempt_number: u32,
    pub outcome_id: Uuid,
    pub evidence_id: Uuid,
    pub outcome: LocalExecutionOutcome,
    pub evidence_fingerprint: [u8; 32],
    pub occurred_at_unix_ms: u64,
}

/// Redacted, untrusted persisted contract metadata.
///
/// This read model intentionally omits provider identities, attestation
/// material, target names, and collision members. It cannot reconstruct
/// `RestartStableIdentityEvidence` and never authorizes a filesystem write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalExecutionContractRecord {
    pub operation_id: Uuid,
    pub vault_id: Uuid,
    pub intent_fingerprint: [u8; 32],
    pub contract_fingerprint: [u8; 32],
    pub collision_member_count: u32,
    pub registered_at_unix_ms: u64,
}

/// Redacted, untrusted persisted outcome metadata from the `SQLite` ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalExecutionOutcomeRecord {
    pub operation_id: Uuid,
    pub attempt_number: u32,
    pub outcome_id: Uuid,
    pub evidence_id: Uuid,
    pub outcome: LocalExecutionOutcome,
    pub evidence_fingerprint: [u8; 32],
    pub non_retryable: bool,
    pub occurred_at_unix_ms: u64,
}

/// Durable publication result for an immutable journal witness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalExecutionWitnessPublicationOutcome {
    Published,
    AlreadyPublished,
}

/// An untrusted outcome claim observed in journal evidence.
///
/// This deliberately has no conversion into [`LocalExecutionOutcome`].  Only
/// the authoritative Step 4 classifier may turn revalidated platform evidence
/// into a final execution outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UntrustedLocalExecutionOutcomeClaim {
    VerifiedApplied,
    VerifiedNotApplied,
    WriteOutcomeUnknown,
    NeedsReconcile,
}

impl UntrustedLocalExecutionOutcomeClaim {
    const fn from_witness(outcome: LocalExecutionOutcome) -> Self {
        match outcome {
            LocalExecutionOutcome::VerifiedApplied => Self::VerifiedApplied,
            LocalExecutionOutcome::VerifiedNotApplied => Self::VerifiedNotApplied,
            LocalExecutionOutcome::WriteOutcomeUnknown => Self::WriteOutcomeUnknown,
            LocalExecutionOutcome::NeedsReconcile => Self::NeedsReconcile,
        }
    }
}

/// A conservative observation after comparing a fresh binding, the immutable
/// `SQLite` ledger, and journal witnesses.  It is not an execution decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalExecutionRecoveryObservation {
    BoundaryWithoutWitness,
    PreSideEffectWitnessOnly,
    OutcomeWitnessPendingLedger {
        claim: UntrustedLocalExecutionOutcomeClaim,
    },
    OutcomeWitnessAndLedgerMatch {
        claim: UntrustedLocalExecutionOutcomeClaim,
    },
}

pub struct SyncStore {
    connection: Connection,
    database_path: PathBuf,
    vault_id: Uuid,
    _lease_file: std::fs::File,
    _private_root: Dir,
    _vault_directory: Dir,
    execution_journal: SyncExecutionJournal,
}

#[derive(Clone, Copy)]
pub(crate) enum PrivateStoragePolicy {
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
        let execution_journal = SyncExecutionJournal::open(
            &private_root,
            canonical_app_root,
            &vault_directory,
            vault_id,
            policy,
        )?;
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
        migrate(&mut connection, vault_id)?;
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
            execution_journal,
        };
        let _ = load_state(&store.connection, store.vault_id)?;
        if !store.local_execution_journal_outcomes_are_exact()? {
            return Err(Error::InvalidSchema);
        }
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

    /// Registers the complete immutable local execution contract atomically.
    ///
    /// This `SQLite` ledger registration is not filesystem mutation authority.
    /// A held-runtime token cannot reach this API: callers must supply the
    /// complete verifier-issued `DurableExecutionBinding` from Step 1.
    ///
    /// # Errors
    /// Returns a collision for any same operation ID with differing immutable
    /// facts, and rolls back every inserted row if the contract is incomplete.
    #[allow(clippy::too_many_lines)]
    pub fn register_local_execution_contract(
        &mut self,
        binding: &DurableExecutionBinding,
        registered_at_unix_ms: u64,
    ) -> Result<LocalExecutionRegistrationOutcome> {
        let projection = binding.persistence_projection();
        if projection.vault_id != self.vault_id {
            return Err(Error::InvalidLocalExecutionEvidence);
        }
        let registered_at = u64_to_i64(registered_at_unix_ms)?;
        let transaction = self.connection.transaction()?;
        if let Some((vault_id, intent, contract, stored_registered_at)) = transaction
            .query_row(
                "SELECT vault_id, intent_fingerprint, contract_fingerprint, registered_at_unix_ms
                 FROM local_execution_contracts WHERE operation_id = ?1",
                [projection.operation_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
        {
            if vault_id == projection.vault_id.to_string()
                && intent == projection.intent_fingerprint
                && contract == projection.contract_fingerprint
                && stored_registered_at == registered_at
            {
                // Header equality is only an index into the immutable
                // contract.  Re-read the complete persisted projection so a
                // child-row rewrite is rejected on the live idempotent path;
                // stable identity is compared while the first attestation is
                // intentionally retained as audit material.
                if !local_execution_operation_rows_are_semantically_valid(
                    &transaction,
                    self.vault_id,
                    projection.operation_id,
                )? {
                    return Err(Error::LocalExecutionJournalMismatch);
                }
                transaction.commit()?;
                return Ok(LocalExecutionRegistrationOutcome::AlreadyPresent);
            }
            return Err(Error::LocalExecutionCollision);
        }
        // A local contract is a contract-first proof.  It must never be
        // registered after R3 has already begun (or completed) the same
        // operation, otherwise a fresh v6 record could retroactively bless
        // historical public work.  This check is deliberately after the
        // exact-idempotency branch above, so an already registered contract
        // remains restart-safe and idempotent.
        let r3_history_exists: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM mutation_intents WHERE operation_id = ?1
                 UNION ALL SELECT 1 FROM mutation_state WHERE operation_id = ?1
                 UNION ALL SELECT 1 FROM mutation_verification_evidence WHERE operation_id = ?1
                 UNION ALL SELECT 1 FROM mutation_events WHERE operation_id = ?1
                 UNION ALL SELECT 1 FROM change_batch_mutations WHERE operation_id = ?1
                 UNION ALL SELECT 1 FROM sync_history WHERE operation_id = ?1
             )",
            [projection.operation_id.to_string()],
            |row| row.get(0),
        )?;
        if r3_history_exists {
            return Err(Error::LocalExecutionCollision);
        }
        let completion_id = local_execution_completion_id(projection.operation_id);
        transaction.execute(
            "INSERT INTO local_execution_contracts(
                operation_id, vault_id, intent_fingerprint, contract_fingerprint,
                target_name, target_collision_key, collision_member_count,
                collision_snapshot_fingerprint, completion_id, registered_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                projection.operation_id.to_string(),
                projection.vault_id.to_string(),
                projection.intent_fingerprint.as_slice(),
                projection.contract_fingerprint.as_slice(),
                projection.target_name,
                projection.target_collision_key,
                i64::from(projection.collision_member_count),
                projection.collision_snapshot_fingerprint.as_slice(),
                completion_id,
                registered_at,
            ],
        )?;
        for identity in [
            projection.vault_root,
            projection.source_parent,
            projection.source_object,
            projection.destination_parent,
            projection.collision_parent_start,
            projection.collision_parent_end,
        ] {
            insert_local_execution_identity(&transaction, projection.operation_id, &identity)?;
        }
        for (ordinal, member) in projection.collision_members.iter().enumerate() {
            transaction.execute(
                "INSERT INTO local_execution_collision_members(
                    operation_id, ordinal, name, collision_key, evidence_version,
                    evidence_kind, object_kind, provider_id, object_id, attestation,
                    stable_identity_fingerprint
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    projection.operation_id.to_string(),
                    i64::try_from(ordinal).map_err(|_| Error::InvalidLocalExecutionEvidence)?,
                    member.name,
                    member.collision_key,
                    i64::from(member.identity.version),
                    i64::from(member.identity.kind),
                    i64::from(member.identity.object_kind),
                    member.identity.provider_id,
                    member.identity.object_id,
                    member.identity.attestation,
                    member.identity.stable_identity_fingerprint.as_slice(),
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO local_execution_contract_completions(completion_id, operation_id, completed_at_unix_ms)
             VALUES (?1, ?2, ?3)",
            params![completion_id, projection.operation_id.to_string(), registered_at],
        )?;
        transaction.commit()?;
        Ok(LocalExecutionRegistrationOutcome::Registered)
    }

    /// Reads redacted, untrusted local execution metadata only.
    ///
    /// The returned data cannot reconstruct verifier-issued identity evidence
    /// and does not authorize execution.
    ///
    /// # Errors
    /// Returns an invalid-operation or database error.
    pub fn local_execution_contract(
        &self,
        operation_id: Uuid,
    ) -> Result<Option<LocalExecutionContractRecord>> {
        if operation_id.is_nil() {
            return Err(Error::LocalExecutionNotFound);
        }
        self.connection
            .query_row(
                "SELECT vault_id, intent_fingerprint, contract_fingerprint, collision_member_count, registered_at_unix_ms
                 FROM local_execution_contracts WHERE operation_id = ?1",
                [operation_id.to_string()],
                |row| {
                    let vault_id = parse_uuid(&row.get::<_, String>(0)?).map_err(to_sql_error)?;
                    let count: i64 = row.get(3)?;
                    let timestamp: i64 = row.get(4)?;
                    Ok(LocalExecutionContractRecord {
                        operation_id,
                        vault_id,
                        intent_fingerprint: blob32(row.get(1)?).map_err(to_sql_error)?,
                        contract_fingerprint: blob32(row.get(2)?).map_err(to_sql_error)?,
                        collision_member_count: u32::try_from(count).map_err(|_| to_sql_error(Error::InvalidSchema))?,
                        registered_at_unix_ms: u64::try_from(timestamp).map_err(|_| to_sql_error(Error::InvalidSchema))?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Appends one attempt boundary exactly once for a complete contract.
    ///
    /// It is ledger evidence only and never authorizes a write.
    ///
    /// # Errors
    /// Returns a collision, absent contract, invalid boundary, or database error.
    pub fn append_local_execution_attempt_boundary(
        &mut self,
        boundary: &LocalExecutionAttemptBoundary,
    ) -> Result<LocalExecutionRegistrationOutcome> {
        if boundary.operation_id.is_nil() || boundary.boundary_id.is_nil() {
            return Err(Error::InvalidLocalExecutionEvidence);
        }
        let occurred_at = u64_to_i64(boundary.occurred_at_unix_ms)?;
        let transaction = self.connection.transaction()?;
        if let Some((boundary_id, fingerprint, timestamp)) = transaction
            .query_row(
                "SELECT boundary_id, contract_fingerprint, occurred_at_unix_ms
                 FROM local_execution_attempt_boundaries WHERE operation_id = ?1 AND attempt_number = ?2",
                params![boundary.operation_id.to_string(), i64::from(boundary.attempt_number)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, i64>(2)?)),
            )
            .optional()?
        {
            if boundary_id == boundary.boundary_id.to_string()
                && fingerprint == boundary.contract_fingerprint.as_bytes()
                && timestamp == occurred_at
            {
                transaction.commit()?;
                return Ok(LocalExecutionRegistrationOutcome::AlreadyPresent);
            }
            return Err(Error::LocalExecutionCollision);
        }
        let binding_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_execution_contracts WHERE operation_id = ?1)",
            [boundary.operation_id.to_string()],
            |row| row.get(0),
        )?;
        if !binding_exists {
            return Err(Error::LocalExecutionNotFound);
        }
        if transaction
            .query_row(
                "SELECT 1 FROM local_execution_attempt_boundaries WHERE boundary_id = ?1",
                [boundary.boundary_id.to_string()],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(Error::LocalExecutionCollision);
        }
        transaction.execute(
            "INSERT INTO local_execution_attempt_boundaries(operation_id, attempt_number, boundary_id, contract_fingerprint, occurred_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                boundary.operation_id.to_string(),
                i64::from(boundary.attempt_number),
                boundary.boundary_id.to_string(),
                boundary.contract_fingerprint.as_bytes().as_slice(),
                occurred_at,
            ],
        )?;
        transaction.commit()?;
        Ok(LocalExecutionRegistrationOutcome::Registered)
    }

    /// Appends one observed local execution outcome exactly once.
    ///
    /// No outcome is inferred by this API. `WriteOutcomeUnknown` is stored
    /// non-retryable by schema and is deliberately not connected to scheduling.
    ///
    /// # Errors
    /// Returns a collision, absent boundary/contract, invalid outcome, or database error.
    pub(crate) fn append_local_execution_attempt_outcome(
        &mut self,
        outcome: &LocalExecutionAttemptOutcome,
    ) -> Result<LocalExecutionRegistrationOutcome> {
        let transaction = self.connection.transaction()?;
        let boundary: (Uuid, u64) = transaction
            .query_row(
                "SELECT boundary_id, occurred_at_unix_ms
                   FROM local_execution_attempt_boundaries
                  WHERE operation_id = ?1 AND attempt_number = ?2",
                params![
                    outcome.operation_id.to_string(),
                    i64::from(outcome.attempt_number)
                ],
                |row| {
                    Ok((
                        parse_uuid(&row.get::<_, String>(0)?).map_err(to_sql_error)?,
                        u64::try_from(row.get::<_, i64>(1)?)
                            .map_err(|_| to_sql_error(Error::InvalidSchema))?,
                    ))
                },
            )
            .optional()?
            .ok_or(Error::LocalExecutionNotFound)?;
        let occurred_at =
            Self::validate_local_execution_outcome_for_boundary(outcome, boundary.0, boundary.1)?;
        if let Some((outcome_id, evidence_id, stored_outcome, evidence, non_retryable, timestamp)) = transaction
            .query_row(
                "SELECT outcome_id, evidence_id, outcome, evidence_fingerprint, non_retryable, occurred_at_unix_ms
                 FROM local_execution_attempt_outcomes WHERE operation_id = ?1 AND attempt_number = ?2",
                params![outcome.operation_id.to_string(), i64::from(outcome.attempt_number)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?)),
            )
            .optional()?
        {
            if outcome_id == outcome.outcome_id.to_string()
                && evidence_id == outcome.evidence_id.to_string()
                && stored_outcome == outcome.outcome.as_str()
                && evidence == outcome.evidence_fingerprint
                && non_retryable == i64::from(outcome.outcome.non_retryable())
                && timestamp == occurred_at
            {
                transaction.commit()?;
                return Ok(LocalExecutionRegistrationOutcome::AlreadyPresent);
            }
            return Err(Error::LocalExecutionCollision);
        }
        let fingerprint: Vec<u8> = transaction
            .query_row(
                "SELECT contract_fingerprint FROM local_execution_contracts WHERE operation_id = ?1",
                [outcome.operation_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(Error::LocalExecutionNotFound)?;
        if transaction
            .query_row(
                "SELECT 1 FROM local_execution_attempt_outcomes WHERE outcome_id = ?1",
                [outcome.outcome_id.to_string()],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(Error::LocalExecutionCollision);
        }
        if transaction
            .query_row(
                "SELECT 1 FROM local_execution_attempt_outcomes WHERE evidence_id = ?1",
                [outcome.evidence_id.to_string()],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(Error::LocalExecutionCollision);
        }
        transaction.execute(
            "INSERT INTO local_execution_attempt_outcomes(
                operation_id, attempt_number, outcome_id, evidence_id, outcome, contract_fingerprint,
                evidence_fingerprint, non_retryable, occurred_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                outcome.operation_id.to_string(),
                i64::from(outcome.attempt_number),
                outcome.outcome_id.to_string(),
                outcome.evidence_id.to_string(),
                outcome.outcome.as_str(),
                fingerprint,
                outcome.evidence_fingerprint.as_slice(),
                i64::from(outcome.outcome.non_retryable()),
                occurred_at,
            ],
        )?;
        transaction.commit()?;
        Ok(LocalExecutionRegistrationOutcome::Registered)
    }

    /// Reads redacted, untrusted attempt outcome metadata only.
    ///
    /// # Errors
    /// Returns an invalid-operation or database error.
    pub fn local_execution_attempt_outcome(
        &self,
        operation_id: Uuid,
        attempt_number: u32,
    ) -> Result<Option<LocalExecutionOutcomeRecord>> {
        if operation_id.is_nil() {
            return Err(Error::LocalExecutionNotFound);
        }
        self.connection.query_row(
            "SELECT outcome_id, evidence_id, outcome, evidence_fingerprint, non_retryable, occurred_at_unix_ms
             FROM local_execution_attempt_outcomes WHERE operation_id = ?1 AND attempt_number = ?2",
            params![operation_id.to_string(), i64::from(attempt_number)],
            |row| {
                let outcome = LocalExecutionOutcome::parse(&row.get::<_, String>(2)?)
                    .map_err(to_sql_error)?;
                let non_retryable = row.get::<_, i64>(4)?;
                if !matches!(non_retryable, 0 | 1)
                    || (non_retryable == 1) != outcome.non_retryable()
                {
                    return Err(to_sql_error(Error::InvalidSchema));
                }
                Ok(LocalExecutionOutcomeRecord {
                    operation_id,
                    attempt_number,
                    outcome_id: parse_uuid(&row.get::<_, String>(0)?).map_err(to_sql_error)?,
                    evidence_id: parse_uuid(&row.get::<_, String>(1)?).map_err(to_sql_error)?,
                    outcome,
                    evidence_fingerprint: blob32(row.get(3)?).map_err(to_sql_error)?,
                    non_retryable: non_retryable == 1,
                    occurred_at_unix_ms: u64::try_from(row.get::<_, i64>(5)?)
                        .map_err(|_| to_sql_error(Error::InvalidSchema))?,
                })
            },
        ).optional().map_err(Into::into)
    }

    /// Publishes the immutable witness immediately before an approved external
    /// side-effect window.  Both the exact contract and exact attempt boundary
    /// must already be present in the append-only `SQLite` ledger.
    ///
    /// This is journal evidence only and does not authorize the side effect.
    /// # Errors
    /// Returns an error when the binding or boundary is not the exact durable
    /// ledger record, or publication/private-storage validation fails.
    pub fn publish_local_execution_pre_side_effect_witness(
        &self,
        binding: &DurableExecutionBinding,
        boundary: &LocalExecutionAttemptBoundary,
        created_at_unix_ms: u64,
    ) -> Result<LocalExecutionWitnessPublicationOutcome> {
        // The pre-side-effect witness has one immutable boundary timestamp.
        // Letting the caller seal a second timestamp creates a journal value
        // that the R3.5 bridge can never reconstruct, permanently blocking an
        // otherwise exact attempt.  Reject before witness construction or any
        // journal file creation.
        if created_at_unix_ms != boundary.occurred_at_unix_ms {
            return Err(Error::InvalidLocalExecutionEvidence);
        }
        let witness = self.exact_execution_witness(binding, boundary, created_at_unix_ms)?;
        Ok(Self::publication_outcome(
            self.execution_journal.publish_pre(&witness)?,
        ))
    }

    /// Publishes an immutable post-window outcome witness.  The outcome is not
    /// inserted into `SQLite` by this method, preserving the crash boundary
    /// between witness publication and the Step 2 append-only ledger outcome.
    /// # Errors
    /// Returns an error unless the identical binding, boundary, and prior pre
    /// witness are present, or when journal publication/private storage fails.
    #[allow(dead_code)] // Reserved generic unbridged local-outcome publication path.
    pub(crate) fn publish_local_execution_outcome_witness(
        &self,
        binding: &DurableExecutionBinding,
        boundary: &LocalExecutionAttemptBoundary,
        outcome: &LocalExecutionAttemptOutcome,
    ) -> Result<LocalExecutionWitnessPublicationOutcome> {
        self.publish_local_execution_outcome_witness_with_r3_evidence(
            binding, boundary, outcome, None,
        )
    }

    fn publish_local_execution_outcome_witness_with_r3_evidence(
        &self,
        binding: &DurableExecutionBinding,
        boundary: &LocalExecutionAttemptBoundary,
        outcome: &LocalExecutionAttemptOutcome,
        r3_mutation_evidence_fingerprint: Option<[u8; 32]>,
    ) -> Result<LocalExecutionWitnessPublicationOutcome> {
        if outcome.operation_id != boundary.operation_id
            || outcome.attempt_number != boundary.attempt_number
        {
            return Err(Error::InvalidLocalExecutionEvidence);
        }
        let _ = Self::validate_local_execution_outcome_for_boundary(
            outcome,
            boundary.boundary_id,
            boundary.occurred_at_unix_ms,
        )?;
        let expected_pre =
            self.exact_execution_witness(binding, boundary, boundary.occurred_at_unix_ms)?;
        let published_pre = self
            .execution_journal
            .read_pre(outcome.operation_id, outcome.attempt_number)?
            .ok_or(Error::LocalExecutionJournalMismatch)?;
        Self::validate_journal_witness_contract(&published_pre, &expected_pre)?;
        let witness = OutcomeWitness {
            pre: published_pre,
            outcome_id: outcome.outcome_id,
            evidence_id: outcome.evidence_id,
            outcome: outcome.outcome,
            evidence_fingerprint: outcome.evidence_fingerprint,
            r3_mutation_evidence_fingerprint,
            created_at_unix_ms: outcome.occurred_at_unix_ms,
        };
        let existing_witness = self
            .execution_journal
            .read_outcome(outcome.operation_id, outcome.attempt_number)?;
        let ledger =
            self.local_execution_attempt_outcome(outcome.operation_id, outcome.attempt_number)?;
        if let Some(existing) = existing_witness.as_ref() {
            if existing != &witness {
                return Err(Error::LocalExecutionJournalCollision);
            }
        }
        if let Some(ledger) = ledger.as_ref() {
            if existing_witness.is_none()
                || !self.ledger_outcome_matches(
                    ledger,
                    &witness,
                    *boundary.contract_fingerprint.as_bytes(),
                )?
            {
                return Err(Error::LocalExecutionJournalMismatch);
            }
        }
        Ok(Self::publication_outcome(
            self.execution_journal.publish_outcome(&witness)?,
        ))
    }

    /// Inspects one known attempt using a newly verifier-issued binding.
    ///
    /// The returned observation is deliberately insufficient to classify a
    /// mutation as applied or not applied.  Step 4 must revalidate the platform
    /// state authoritatively before any final action is taken.
    /// # Errors
    /// Returns an error for an absent/mismatched fresh binding or boundary, or
    /// for malformed, insecure, substituted, or conflicting journal evidence.
    pub fn inspect_local_execution_recovery(
        &self,
        binding: &DurableExecutionBinding,
        attempt_number: u32,
    ) -> Result<LocalExecutionRecoveryObservation> {
        let projection = binding.persistence_projection();
        let boundary = self.execution_boundary_for_binding(binding, attempt_number)?;
        let expected = PreSideEffectWitness {
            operation_id: projection.operation_id,
            attempt_number,
            boundary_id: boundary.0,
            boundary_occurred_at_unix_ms: boundary.1,
            intent_fingerprint: projection.intent_fingerprint,
            contract_fingerprint: projection.contract_fingerprint,
            collision_snapshot_fingerprint: projection.collision_snapshot_fingerprint,
            created_at_unix_ms: boundary.1,
        };
        let pre = self
            .execution_journal
            .read_pre(projection.operation_id, attempt_number)?;
        let outcome = self
            .execution_journal
            .read_outcome(projection.operation_id, attempt_number)?;
        let ledger =
            self.local_execution_attempt_outcome(projection.operation_id, attempt_number)?;
        let Some(pre) = pre else {
            if outcome.is_some() || ledger.is_some() {
                return Err(Error::LocalExecutionJournalMismatch);
            }
            return Ok(LocalExecutionRecoveryObservation::BoundaryWithoutWitness);
        };
        Self::validate_journal_witness_contract(&pre, &expected)?;
        let Some(outcome) = outcome else {
            if ledger.is_some() {
                return Err(Error::LocalExecutionJournalMismatch);
            }
            return Ok(LocalExecutionRecoveryObservation::PreSideEffectWitnessOnly);
        };
        let exact_expected = expected.clone();
        let receipt_r3: Option<String> = self
            .connection
            .query_row(
                "SELECT r3_evidence_fingerprint FROM local_execution_r3_bridge_receipts
              WHERE operation_id = ?1 AND attempt_number = ?2",
                params![
                    projection.operation_id.to_string(),
                    i64::from(attempt_number)
                ],
                |row| row.get(0),
            )
            .optional()?;
        let expected_r3 = match receipt_r3 {
            Some(value) => JournalR3Expectation::Bridged(
                parse_canonical_sha256(&value).ok_or(Error::LocalExecutionJournalMismatch)?,
            ),
            None => match outcome.r3_mutation_evidence_fingerprint {
                Some(value) => JournalR3Expectation::AuthoritativePreReceipt(value),
                None => JournalR3Expectation::GenericUnbridged,
            },
        };
        self.exact_journal_pair(
            &exact_expected,
            outcome.outcome_id,
            outcome.evidence_id,
            outcome.outcome,
            outcome.evidence_fingerprint,
            outcome.created_at_unix_ms,
            expected_r3,
        )?;
        match ledger {
            None => Ok(
                LocalExecutionRecoveryObservation::OutcomeWitnessPendingLedger {
                    claim: UntrustedLocalExecutionOutcomeClaim::from_witness(outcome.outcome),
                },
            ),
            Some(ledger)
                if self.ledger_outcome_matches(
                    &ledger,
                    &outcome,
                    projection.contract_fingerprint,
                )? =>
            {
                Ok(
                    LocalExecutionRecoveryObservation::OutcomeWitnessAndLedgerMatch {
                        claim: UntrustedLocalExecutionOutcomeClaim::from_witness(outcome.outcome),
                    },
                )
            }
            Some(_) => Err(Error::LocalExecutionJournalMismatch),
        }
    }

    /// Finalizes one exact verifier-derived outcome in crash-safe order.
    ///
    /// The authoritative classifier must run before this method.  A prior
    /// immutable outcome witness is reused only when it is byte-for-byte the
    /// same logical outcome; a conflicting claim is never repaired.  The
    /// journal is published before the v6 ledger, so a crash in between resumes
    /// by appending the same immutable ledger row without a duplicate outcome.
    ///
    /// This method performs no local/provider side effect.
    ///
    /// # Errors
    /// Returns a redacted mismatch/collision error for any stale binding,
    /// witness, ledger, or purported authoritative decision.
    pub fn finalize_authoritative_local_execution_outcome(
        &mut self,
        binding: &DurableExecutionBinding,
        boundary: &LocalExecutionAttemptBoundary,
        decision: &AuthoritativeFinalOutcome,
    ) -> Result<LocalExecutionRegistrationOutcome> {
        if !decision.matches_binding(binding, boundary) {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        let recorded_at_unix_ms = match self
            .execution_journal
            .read_outcome(boundary.operation_id, boundary.attempt_number)?
        {
            Some(witness) => {
                if witness.outcome_id != decision.outcome_id()
                    || witness.evidence_id != decision.evidence_id()
                    || witness.outcome != decision.outcome()
                    || witness.evidence_fingerprint != decision.evidence_fingerprint()
                    || witness.created_at_unix_ms != decision.recorded_at_unix_ms()
                {
                    return Err(Error::LocalExecutionJournalCollision);
                }
                decision.recorded_at_unix_ms()
            }
            None => decision.recorded_at_unix_ms(),
        };
        let outcome = LocalExecutionAttemptOutcome {
            operation_id: decision.operation_id(),
            attempt_number: decision.attempt_number(),
            outcome_id: decision.outcome_id(),
            evidence_id: decision.evidence_id(),
            outcome: decision.outcome(),
            evidence_fingerprint: decision.evidence_fingerprint(),
            occurred_at_unix_ms: recorded_at_unix_ms,
        };
        self.publish_local_execution_outcome_witness_with_r3_evidence(
            binding,
            boundary,
            &outcome,
            Some(decision.r3_mutation_evidence_fingerprint()),
        )?;
        self.append_local_execution_attempt_outcome(&outcome)
    }

    /// R3.5-only bridge from an authoritative local execution proof to the
    /// frozen R3 cursor gates.
    ///
    /// Unlike [`Self::commit_r3_change_dependency`], this entry point proves
    /// that the exact fresh local binding, Step 3 witness, v6 ledger, sealed
    /// authoritative decision, and existing R3 post-verify evidence agree.
    /// It then delegates to the existing R3 gate without changing its rules.
    /// Journal/ledger/hint evidence alone, `legacy_v3`, and every non-applied
    /// result are withheld.
    ///
    /// # Errors
    /// Returns a redacted error unless every exact R3.5 relation holds.
    #[allow(clippy::too_many_lines)]
    pub fn commit_r3_5_verified_local_execution_dependency(
        &mut self,
        batch_id: Uuid,
        dependency: ChangeBatchDependency,
        binding: &DurableExecutionBinding,
        attempt_number: u32,
        decision: &AuthoritativeFinalOutcome,
    ) -> Result<()> {
        if dependency.operation_id != decision.operation_id()
            || dependency.operation_id != binding.persistence_projection().operation_id
            || decision.attempt_number() != attempt_number
            || decision.outcome() != LocalExecutionOutcome::VerifiedApplied
        {
            return Err(Error::LocalMutationIncomplete);
        }
        let projection = binding.persistence_projection();
        let decision_boundary = LocalExecutionAttemptBoundary {
            operation_id: dependency.operation_id,
            attempt_number,
            boundary_id: decision.boundary_id(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: decision.boundary_occurred_at_unix_ms(),
        };
        if decision.boundary_id().is_nil()
            || decision.evidence_id().is_nil()
            || decision.outcome_id().is_nil()
            || decision.boundary_occurred_at_unix_ms() > i64::MAX as u64
            || decision.recorded_at_unix_ms() > i64::MAX as u64
            || !decision.matches_binding(binding, &decision_boundary)
        {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        // The journal is immutable/no-replace and is the only relation that
        // may be checked outside the SQLite snapshot.  Every local-ledger to
        // R3 relation below is deliberately reread in one transaction.
        let expected_pre = PreSideEffectWitness {
            operation_id: dependency.operation_id,
            attempt_number,
            boundary_id: decision.boundary_id(),
            boundary_occurred_at_unix_ms: decision.boundary_occurred_at_unix_ms(),
            intent_fingerprint: projection.intent_fingerprint,
            contract_fingerprint: projection.contract_fingerprint,
            collision_snapshot_fingerprint: projection.collision_snapshot_fingerprint,
            created_at_unix_ms: decision.boundary_occurred_at_unix_ms(),
        };
        self.exact_journal_pair(
            &expected_pre,
            decision.outcome_id(),
            decision.evidence_id(),
            LocalExecutionOutcome::VerifiedApplied,
            decision.evidence_fingerprint(),
            decision.recorded_at_unix_ms(),
            // Receipt consumption is published only after its SQLite
            // transaction commits.  Until then this is the recoverable
            // authoritative pre-receipt crash state, never cursor authority.
            JournalR3Expectation::AuthoritativePreReceipt(
                decision.r3_mutation_evidence_fingerprint(),
            ),
        )?;
        let transaction = self.connection.transaction()?;
        let persisted_intent =
            load_persisted_mutation_intent(&transaction, dependency.operation_id)?
                .ok_or(Error::MutationNotFound)?;
        validate_mutation_intent(&persisted_intent)?;
        if !mutation_history_is_exact(&transaction, dependency.operation_id)? {
            return Err(Error::LocalMutationIncomplete);
        }
        // This one snapshot checks exact local contract/boundary/outcome and
        // every R3 intent/evidence/state/completion-event fact before it can
        // write either the durable receipt or the batch dependency.
        let r3: Option<LocalBridgeSnapshot> = transaction
            .query_row(
                "SELECT contract.vault_id, contract.intent_fingerprint,
                        contract.contract_fingerprint, contract.collision_snapshot_fingerprint,
                        boundary.boundary_id, boundary.occurred_at_unix_ms,
                        outcome.outcome_id, outcome.evidence_id, outcome.outcome,
                        outcome.evidence_fingerprint, outcome.occurred_at_unix_ms,
                        intent.intent_fingerprint, evidence.attempt_number,
                        evidence.capture_phase, evidence.disposition, evidence.forbidden_side_effect,
                        evidence.evidence_fingerprint, evidence.outcome_code,
                        state.phase, state.disposition, state.outcome_code,
                        state.attempt_number, state.state_version, state.last_evidence_id,
                        event.phase, event.disposition, event.evidence_id, event.outcome_code
                   FROM local_execution_contracts AS contract
                   JOIN local_execution_attempt_boundaries AS boundary
                     ON boundary.operation_id = contract.operation_id
                   JOIN local_execution_attempt_outcomes AS outcome
                     ON outcome.operation_id = boundary.operation_id AND outcome.attempt_number = boundary.attempt_number
                   JOIN mutation_intents AS intent ON intent.operation_id = contract.operation_id
                   JOIN mutation_verification_evidence AS evidence
                     ON evidence.operation_id = intent.operation_id AND evidence.evidence_id = ?3
                   JOIN mutation_state AS state ON state.operation_id = intent.operation_id
                   JOIN mutation_events AS event ON event.operation_id = state.operation_id
                     AND event.attempt_number = state.attempt_number
                     AND event.state_version = state.state_version
                     AND event.evidence_id = state.last_evidence_id
                  WHERE contract.operation_id = ?1 AND boundary.attempt_number = ?2",
                params![
                    dependency.operation_id.to_string(),
                    i64::from(attempt_number),
                    decision.evidence_id().to_string(),
                ],
                |row| {
                    Ok((
                        row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?,
                        row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?, row.get(11)?,
                        row.get(12)?, row.get(13)?, row.get(14)?, row.get(15)?, row.get(16)?, row.get(17)?,
                        row.get(18)?, row.get(19)?, row.get(20)?, row.get(21)?, row.get(22)?, row.get(23)?, row.get(24)?, row.get(25)?, row.get(26)?, row.get(27)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            vault,
            local_intent,
            local_contract,
            local_collision,
            boundary_id,
            boundary_at,
            outcome_id,
            outcome_evidence_id,
            outcome,
            local_evidence,
            outcome_at,
            r3_intent,
            r3_attempt,
            capture_phase,
            evidence_disposition,
            forbidden,
            r3_evidence,
            evidence_outcome_code,
            state_phase,
            state_disposition,
            state_outcome_code,
            state_attempt,
            state_version,
            last_evidence,
            event_phase,
            event_disposition,
            event_evidence,
            event_outcome_code,
        )) = r3
        else {
            return Err(Error::LocalMutationIncomplete);
        };
        let expected_local_intent = local_intent_fingerprint_from_r3_intent(&r3_intent)
            .map_err(|_| Error::InvalidLocalExecutionEvidence)?;
        if parse_uuid(&vault).ok() != Some(projection.vault_id)
            || local_intent.as_slice() != projection.intent_fingerprint
            || local_contract.as_slice() != projection.contract_fingerprint
            || local_collision.as_slice() != projection.collision_snapshot_fingerprint
            || parse_uuid(&boundary_id).ok() != Some(decision.boundary_id())
            || u64::try_from(boundary_at).ok() != Some(decision.boundary_occurred_at_unix_ms())
            || outcome_id != decision.outcome_id().to_string()
            || outcome_evidence_id != decision.evidence_id().to_string()
            || outcome != "VerifiedApplied"
            || local_evidence.as_slice() != decision.evidence_fingerprint()
            || u64::try_from(outcome_at).ok() != Some(decision.recorded_at_unix_ms())
            || local_intent.as_slice() != expected_local_intent
            || persisted_intent.intent_fingerprint != r3_intent
            || u32::try_from(r3_attempt).ok() != Some(attempt_number)
            || capture_phase != "post_verify"
            || evidence_disposition != "verified_applied"
            || forbidden != 0
            || evidence_outcome_code != state_outcome_code
            || state_phase != "completed"
            || state_disposition.as_deref() != Some("verified_applied")
            || u32::try_from(state_attempt).ok() != Some(attempt_number)
            || last_evidence.as_deref() != Some(decision.evidence_id().to_string().as_str())
            || event_phase != "completed"
            || event_disposition.as_deref() != Some("verified_applied")
            || event_evidence.as_deref() != Some(decision.evidence_id().to_string().as_str())
            || event_outcome_code != state_outcome_code
            || parse_canonical_sha256(&r3_evidence)
                != Some(decision.r3_mutation_evidence_fingerprint())
            || !persisted_verified_applied_mutation_evidence_is_exact(
                &transaction,
                decision.evidence_id(),
            )?
        {
            return Err(Error::LocalMutationIncomplete);
        }
        let receipt = BridgeReceiptFacts {
            operation_id: dependency.operation_id,
            attempt_number,
            boundary_id: decision.boundary_id(),
            boundary_occurred_at_unix_ms: decision.boundary_occurred_at_unix_ms(),
            contract_fingerprint: projection.contract_fingerprint,
            outcome_id: decision.outcome_id(),
            evidence_id: decision.evidence_id(),
            local_evidence_fingerprint: decision.evidence_fingerprint(),
            outcome_occurred_at_unix_ms: decision.recorded_at_unix_ms(),
            r3_intent_fingerprint: r3_intent,
            r3_evidence_fingerprint: r3_evidence,
            r3_outcome_code: state_outcome_code,
            dependency_kind: dependency.kind.as_str().to_owned(),
            r3_state_phase: state_phase,
            r3_state_disposition: state_disposition.ok_or(Error::LocalMutationIncomplete)?,
            r3_attempt_number: attempt_number,
            r3_state_version: u64::try_from(state_version).map_err(|_| Error::InvalidSchema)?,
            r3_last_evidence_id: parse_uuid(&last_evidence.ok_or(Error::LocalMutationIncomplete)?)?,
            r3_event_state_version: u64::try_from(state_version)
                .map_err(|_| Error::InvalidSchema)?,
        };
        let consumption = bridge_consumption_witness(&expected_pre, &receipt)?;
        insert_or_require_exact_bridge_receipt(&transaction, &receipt)?;
        insert_or_require_exact_consumption_anchor(&transaction, &receipt)?;
        Self::commit_r3_change_dependency_in_transaction(
            &transaction,
            batch_id,
            dependency,
            decision.evidence_id(),
            true,
        )?;
        transaction.commit()?;
        // This durable no-replace marker is deliberately outside the receipt
        // transaction.  A crash before it is published leaves a recoverable
        // receipt-complete bridge that cannot advance a cursor.  An exact
        // retry rechecks the same receipt/dependency and publishes this exact
        // marker without fabricating either SQLite row.
        self.execution_journal
            .publish_bridge_consumption(&consumption)?;
        Ok(())
    }

    fn publication_outcome(published: bool) -> LocalExecutionWitnessPublicationOutcome {
        if published {
            LocalExecutionWitnessPublicationOutcome::Published
        } else {
            LocalExecutionWitnessPublicationOutcome::AlreadyPublished
        }
    }

    fn validate_local_execution_outcome(outcome: &LocalExecutionAttemptOutcome) -> Result<i64> {
        if outcome.operation_id.is_nil()
            || outcome.outcome_id.is_nil()
            || outcome.evidence_id.is_nil()
        {
            return Err(Error::InvalidLocalExecutionEvidence);
        }
        u64_to_i64(outcome.occurred_at_unix_ms)
    }

    fn validate_local_execution_outcome_for_boundary(
        outcome: &LocalExecutionAttemptOutcome,
        boundary_id: Uuid,
        boundary_occurred_at_unix_ms: u64,
    ) -> Result<i64> {
        if boundary_id.is_nil() {
            return Err(Error::InvalidLocalExecutionEvidence);
        }
        let occurred_at = Self::validate_local_execution_outcome(outcome)?;
        if authoritative_outcome_id(
            outcome.operation_id,
            outcome.attempt_number,
            boundary_id,
            boundary_occurred_at_unix_ms,
            outcome.evidence_id,
            outcome.evidence_fingerprint,
            outcome.outcome,
            outcome.occurred_at_unix_ms,
        ) != outcome.outcome_id
        {
            return Err(Error::InvalidLocalExecutionEvidence);
        }
        Ok(occurred_at)
    }

    fn exact_execution_witness(
        &self,
        binding: &DurableExecutionBinding,
        boundary: &LocalExecutionAttemptBoundary,
        created_at_unix_ms: u64,
    ) -> Result<PreSideEffectWitness> {
        if boundary.operation_id.is_nil()
            || boundary.boundary_id.is_nil()
            || created_at_unix_ms != boundary.occurred_at_unix_ms
        {
            return Err(Error::InvalidLocalExecutionEvidence);
        }
        let projection = binding.persistence_projection();
        if projection.operation_id != boundary.operation_id
            || projection.vault_id != self.vault_id
            || projection.contract_fingerprint != *boundary.contract_fingerprint.as_bytes()
        {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        let (boundary_id, occurred_at) =
            self.execution_boundary_for_binding(binding, boundary.attempt_number)?;
        if boundary_id != boundary.boundary_id || occurred_at != boundary.occurred_at_unix_ms {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        Ok(PreSideEffectWitness {
            operation_id: projection.operation_id,
            attempt_number: boundary.attempt_number,
            boundary_id: boundary.boundary_id,
            boundary_occurred_at_unix_ms: boundary.occurred_at_unix_ms,
            intent_fingerprint: projection.intent_fingerprint,
            contract_fingerprint: projection.contract_fingerprint,
            collision_snapshot_fingerprint: projection.collision_snapshot_fingerprint,
            created_at_unix_ms,
        })
    }

    fn execution_boundary_for_binding(
        &self,
        binding: &DurableExecutionBinding,
        attempt_number: u32,
    ) -> Result<(Uuid, u64)> {
        let projection = binding.persistence_projection();
        if projection.vault_id != self.vault_id {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        let contract: Option<LocalExecutionContractFingerprints> = self.connection.query_row(
            "SELECT vault_id, intent_fingerprint, contract_fingerprint, collision_snapshot_fingerprint
             FROM local_execution_contracts WHERE operation_id = ?1",
            [projection.operation_id.to_string()],
            |row| {
                Ok((
                    parse_uuid(&row.get::<_, String>(0)?).map_err(to_sql_error)?,
                    blob32(row.get(1)?).map_err(to_sql_error)?,
                    blob32(row.get(2)?).map_err(to_sql_error)?,
                    blob32(row.get(3)?).map_err(to_sql_error)?,
                ))
            },
        ).optional()?;
        let Some((vault_id, intent, contract, collision)) = contract else {
            return Err(Error::LocalExecutionNotFound);
        };
        if vault_id != projection.vault_id
            || intent != projection.intent_fingerprint
            || contract != projection.contract_fingerprint
            || collision != projection.collision_snapshot_fingerprint
        {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        self.connection
            .query_row(
                "SELECT boundary_id, occurred_at_unix_ms FROM local_execution_attempt_boundaries
             WHERE operation_id = ?1 AND attempt_number = ?2 AND contract_fingerprint = ?3",
                params![
                    projection.operation_id.to_string(),
                    i64::from(attempt_number),
                    projection.contract_fingerprint.as_slice(),
                ],
                |row| {
                    Ok((
                        parse_uuid(&row.get::<_, String>(0)?).map_err(to_sql_error)?,
                        u64::try_from(row.get::<_, i64>(1)?)
                            .map_err(|_| to_sql_error(Error::InvalidSchema))?,
                    ))
                },
            )
            .optional()?
            .ok_or(Error::LocalExecutionNotFound)
    }

    fn validate_journal_witness_contract(
        actual: &PreSideEffectWitness,
        expected: &PreSideEffectWitness,
    ) -> Result<()> {
        if actual.operation_id != expected.operation_id
            || actual.attempt_number != expected.attempt_number
            || actual.boundary_id != expected.boundary_id
            || actual.boundary_occurred_at_unix_ms != expected.boundary_occurred_at_unix_ms
            || actual.intent_fingerprint != expected.intent_fingerprint
            || actual.contract_fingerprint != expected.contract_fingerprint
            || actual.collision_snapshot_fingerprint != expected.collision_snapshot_fingerprint
            || actual.created_at_unix_ms != expected.created_at_unix_ms
        {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        Ok(())
    }

    /// Reads the two independently persisted immutable files and proves the
    /// sealed pair.  An embedded `pre` is never accepted as a substitute for
    /// the separately named `.pre` witness.
    #[allow(clippy::too_many_arguments)]
    fn exact_journal_pair(
        &self,
        expected_pre: &PreSideEffectWitness,
        outcome_id: Uuid,
        evidence_id: Uuid,
        outcome: LocalExecutionOutcome,
        evidence_fingerprint: [u8; 32],
        created_at_unix_ms: u64,
        expected_r3_evidence_fingerprint: JournalR3Expectation,
    ) -> Result<OutcomeWitness> {
        exact_journal_pair(
            &self.execution_journal,
            expected_pre,
            outcome_id,
            evidence_id,
            outcome,
            evidence_fingerprint,
            created_at_unix_ms,
            expected_r3_evidence_fingerprint,
        )
    }

    fn ledger_outcome_matches(
        &self,
        ledger: &LocalExecutionOutcomeRecord,
        witness: &OutcomeWitness,
        contract_fingerprint: [u8; 32],
    ) -> Result<bool> {
        if ledger.outcome_id != witness.outcome_id
            || ledger.evidence_id != witness.evidence_id
            || ledger.outcome != witness.outcome
            || ledger.evidence_fingerprint != witness.evidence_fingerprint
            || ledger.occurred_at_unix_ms != witness.created_at_unix_ms
        {
            return Ok(false);
        }
        let stored: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT contract_fingerprint FROM local_execution_attempt_outcomes
             WHERE operation_id = ?1 AND attempt_number = ?2",
                params![
                    ledger.operation_id.to_string(),
                    i64::from(ledger.attempt_number)
                ],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored.as_deref() == Some(contract_fingerprint.as_slice()))
    }

    /// On every reopen, every durable local outcome must be independently
    /// witnessed.  The journal is compared to the ledger; it is never used to
    /// manufacture a row or authorize a cursor by itself.
    #[allow(clippy::too_many_lines)]
    fn local_execution_journal_outcomes_are_exact(&self) -> Result<bool> {
        let mut statement = self.connection.prepare(
            "SELECT outcome.operation_id, outcome.attempt_number, boundary.boundary_id,
                    boundary.occurred_at_unix_ms, boundary.contract_fingerprint,
                    outcome.outcome_id, outcome.evidence_id, outcome.outcome,
                    outcome.evidence_fingerprint, outcome.occurred_at_unix_ms
               FROM local_execution_attempt_outcomes AS outcome
               JOIN local_execution_attempt_boundaries AS boundary
                 ON boundary.operation_id = outcome.operation_id
                AND boundary.attempt_number = outcome.attempt_number",
        )?;
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Vec<u8>>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })? {
            let (
                operation,
                attempt,
                boundary,
                boundary_at,
                contract,
                outcome_id,
                evidence_id,
                outcome,
                evidence_fingerprint,
                outcome_at,
            ) = row?;
            let (
                Ok(operation),
                Ok(attempt),
                Ok(boundary),
                Ok(boundary_at),
                Ok(contract),
                Ok(outcome_id),
                Ok(evidence_id),
                Ok(outcome),
                Ok(evidence_fingerprint),
                Ok(outcome_at),
            ) = (
                parse_uuid(&operation),
                u32::try_from(attempt),
                parse_uuid(&boundary),
                u64::try_from(boundary_at),
                blob32(contract),
                parse_uuid(&outcome_id),
                parse_uuid(&evidence_id),
                LocalExecutionOutcome::parse(&outcome),
                blob32(evidence_fingerprint),
                u64::try_from(outcome_at),
            )
            else {
                return Ok(false);
            };
            let expected_pre = PreSideEffectWitness {
                operation_id: operation,
                attempt_number: attempt,
                boundary_id: boundary,
                boundary_occurred_at_unix_ms: boundary_at,
                // This fingerprint is fixed by the matching contract row;
                // read it rather than trusting the outcome witness.
                intent_fingerprint: self.connection.query_row(
                    "SELECT intent_fingerprint FROM local_execution_contracts WHERE operation_id = ?1",
                    [operation.to_string()],
                    |row| blob32(row.get(0)?).map_err(to_sql_error),
                )?,
                contract_fingerprint: contract,
                collision_snapshot_fingerprint: self.connection.query_row(
                    "SELECT collision_snapshot_fingerprint FROM local_execution_contracts WHERE operation_id = ?1",
                    [operation.to_string()],
                    |row| blob32(row.get(0)?).map_err(to_sql_error),
                )?,
                created_at_unix_ms: boundary_at,
            };
            let pre = self
                .execution_journal
                .read_pre(operation, attempt)?
                .ok_or(Error::LocalExecutionJournalMismatch)?;
            Self::validate_journal_witness_contract(&pre, &expected_pre)?;
            let receipt_marker_facts: Option<(String, Vec<u8>, String, String, String)> = self
                .connection
                .query_row(
                    "SELECT receipt_id, receipt_fingerprint, r3_intent_fingerprint,
                            r3_evidence_fingerprint, dependency_kind
                       FROM local_execution_r3_bridge_receipts
                      WHERE operation_id = ?1 AND attempt_number = ?2",
                    params![operation.to_string(), i64::from(attempt)],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            let expected_r3 = match receipt_marker_facts.as_ref() {
                Some((_, _, _, value, _)) => match parse_canonical_sha256(value) {
                    Some(value) => JournalR3Expectation::Bridged(value),
                    None => return Ok(false),
                },
                None => match self.execution_journal.read_outcome(operation, attempt)? {
                    Some(witness) => match witness.r3_mutation_evidence_fingerprint {
                        Some(value) => JournalR3Expectation::AuthoritativePreReceipt(value),
                        None => JournalR3Expectation::GenericUnbridged,
                    },
                    None => return Ok(false),
                },
            };
            let pair = self
                .exact_journal_pair(
                    &expected_pre,
                    outcome_id,
                    evidence_id,
                    outcome,
                    evidence_fingerprint,
                    outcome_at,
                    expected_r3,
                )
                .ok();
            if pair.is_none()
                || authoritative_outcome_id(
                    operation,
                    attempt,
                    boundary,
                    boundary_at,
                    evidence_id,
                    evidence_fingerprint,
                    outcome,
                    outcome_at,
                ) != outcome_id
            {
                return Ok(false);
            }
            #[allow(clippy::single_match_else)]
            // Retain explicit fail-closed absent-receipt branch.
            match receipt_marker_facts {
                Some((receipt_id, receipt_fingerprint, r3_intent, r3_evidence, kind)) => {
                    let (
                        Ok(receipt_id),
                        Ok(receipt_fingerprint),
                        Some(r3_intent),
                        Some(r3_evidence),
                        Ok(kind),
                    ) = (
                        parse_uuid(&receipt_id),
                        blob32(receipt_fingerprint),
                        parse_canonical_sha256(&r3_intent),
                        parse_canonical_sha256(&r3_evidence),
                        bridge_dependency_kind_code(&kind),
                    )
                    else {
                        return Ok(false);
                    };
                    let expected = BridgeConsumptionWitness {
                        pre: expected_pre.clone(),
                        receipt_id,
                        receipt_fingerprint,
                        outcome_id,
                        evidence_id,
                        local_evidence_fingerprint: evidence_fingerprint,
                        outcome_occurred_at_unix_ms: outcome_at,
                        r3_intent_fingerprint: r3_intent,
                        r3_evidence_fingerprint: r3_evidence,
                        dependency_kind: kind,
                    };
                    // No marker is the explicitly recoverable receipt-commit /
                    // marker-publish crash window.  A present marker must be
                    // exact; a substituted marker makes reopening fail closed.
                    if let Some(actual) = self
                        .execution_journal
                        .read_bridge_consumption(operation, attempt)?
                    {
                        if actual != expected {
                            return Ok(false);
                        }
                    }
                }
                None => {
                    // A consumption marker is published only after receipt
                    // commit.  It survives batch cleanup, so receipt deletion
                    // can never downgrade this into a pre-receipt crash.
                    if self
                        .execution_journal
                        .read_bridge_consumption(operation, attempt)?
                        .is_some()
                    {
                        return Ok(false);
                    }
                    let orphaned_anchor: bool = self.connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM local_execution_r3_consumption_anchors
                          WHERE operation_id = ?1 AND attempt_number = ?2)",
                        params![operation.to_string(), i64::from(attempt)],
                        |row| row.get(0),
                    )?;
                    if orphaned_anchor {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
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
            validate_remote_existing_blocked_initial_needs_reconcile(intent, evidence)?;
        } else if initial_evidence.is_some() {
            return Err(Error::InvalidStateTransition);
        }

        let transaction = self.connection.transaction()?;
        if load_mutation_identity(&transaction, intent.operation_id)?.is_some() {
            let persisted = load_persisted_mutation_intent(&transaction, intent.operation_id)?
                .ok_or(Error::InvalidSchema)?;
            // The indexed fingerprint/marker are only a collision lookup.
            // Reconstruct and validate the full immutable preimage before an
            // idempotent result, so neither a stale hash nor a coherent hash
            // rewrite can turn a different caller intent into AlreadyPresent.
            let mut caller_preimage = intent.clone();
            // Registration time is an observation timestamp, deliberately
            // excluded from the immutable intent fingerprint so a restart can
            // replay the same blocked intent at a later time.  Every actual
            // immutable intent field remains an exact equality requirement.
            caller_preimage.registered_at_unix_ms = persisted.registered_at_unix_ms;
            if validate_mutation_intent(&persisted).is_err() || persisted != caller_preimage {
                return Err(Error::MutationCollision);
            }
            // A blocked operation has no executable recovery path.  An exact
            // re-registration therefore must not turn a forged initial
            // `NeedsReconcile` history into an idempotent success.
            if blocked && !mutation_history_is_exact(&transaction, intent.operation_id)? {
                return Err(Error::LocalMutationIncomplete);
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

    /// Claims a durable intent or an exact, due retry using the caller's expected state version.
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
            || now_unix_ms < state.updated_at_unix_ms
        {
            return Err(Error::InvalidStateTransition);
        }
        let next = MutationState {
            operation_id,
            phase: MutationPhase::Running,
            // A retry is only possible after an exact durable retry outcome,
            // and consumes exactly one checked attempt number.  The executor
            // still has to revalidate the provider/local preconditions before
            // any replay; this claim alone is never side-effect authority.
            attempt_number: state
                .attempt_number
                .checked_add(u32::from(retry))
                .ok_or(Error::InvalidSchema)?,
            state_version: state
                .state_version
                .checked_add(1)
                .ok_or(Error::InvalidSchema)?,
            disposition: None,
            next_attempt_at_unix_ms: None,
            retry_mode: None,
            resume_reference: None,
            last_evidence_id: None,
            outcome_code: None,
            updated_at_unix_ms: now_unix_ms,
        };
        if retry && !mutation_history_is_exact(&transaction, operation_id)? {
            return Err(Error::LocalMutationIncomplete);
        }
        if retry && !mutation_retry_contract_matches_state(&transaction, &state)? {
            return Err(Error::LocalMutationIncomplete);
        }
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
        if evidence.captured_at_unix_ms < state.updated_at_unix_ms {
            return Err(Error::InvalidStateTransition);
        }
        if matches!(transition, MutationOutcomeTransition::VerifiedApplied) {
            validate_verified_applied_evidence(&transaction, evidence)?;
        }
        if matches!(
            transition,
            MutationOutcomeTransition::VerifiedNotApplied { .. }
                | MutationOutcomeTransition::RetrySafe { .. }
        ) {
            let intent = load_persisted_mutation_intent(&transaction, operation_id)?
                .ok_or(Error::MutationNotFound)?;
            validate_mutation_intent(&intent)?;
            validate_verified_applied_evidence_against_intent(&intent, evidence)?;
        }
        let (phase, disposition, next_attempt, retry_mode, resume_reference) =
            transition_target(state.phase, evidence, transition)?;
        insert_mutation_evidence(&transaction, evidence)?;
        let next = MutationState {
            operation_id,
            phase,
            attempt_number: state.attempt_number,
            state_version: state
                .state_version
                .checked_add(1)
                .ok_or(Error::InvalidSchema)?,
            disposition: Some(disposition),
            next_attempt_at_unix_ms: next_attempt,
            retry_mode,
            resume_reference,
            last_evidence_id: Some(evidence.evidence_id),
            outcome_code: evidence.outcome_code.clone(),
            updated_at_unix_ms: evidence.captured_at_unix_ms,
        };
        if next.phase == MutationPhase::RetryScheduled {
            insert_mutation_retry_contract(&transaction, &next, evidence)?;
        }
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
        let transaction = self.connection.transaction()?;
        Self::commit_r3_change_dependency_in_transaction(
            &transaction,
            batch_id,
            dependency,
            evidence_id,
            false,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The cursor gate's sole private implementation.  A public R3 caller may
    /// never use this path for an operation with a v6 local contract: only the
    /// R3.5 bridge supplies `allow_verified_local_contract` after binding its
    /// sealed proof in the same database transaction.
    fn commit_r3_change_dependency_in_transaction(
        transaction: &Transaction<'_>,
        batch_id: Uuid,
        dependency: ChangeBatchDependency,
        evidence_id: Uuid,
        allow_verified_local_contract: bool,
    ) -> Result<()> {
        if batch_id.is_nil() || dependency.operation_id.is_nil() || evidence_id.is_nil() {
            return Err(Error::InvalidStateTransition);
        }
        let batch = load_change_batch(transaction)?.ok_or(Error::NoActiveBatch)?;
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
        let local_contract_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_execution_contracts WHERE operation_id = ?1)",
            [dependency.operation_id.to_string()],
            |row| row.get(0),
        )?;
        if local_contract_exists && !allow_verified_local_contract {
            return Err(Error::LocalMutationIncomplete);
        }
        let (_, operation_kind) = load_mutation_kind(transaction, dependency.operation_id)?
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
        require_exact_r3_completion_evidence(transaction, dependency.operation_id, evidence_id)?;
        if LocalMutationState::parse(&row.3)? == LocalMutationState::Committed {
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
        Ok(())
    }

    /// Advances the cursor only after every typed R3 dependency has exact evidence.
    ///
    /// # Errors
    /// Returns missing-batch, incomplete-dependency, changed-cursor, or database errors.
    #[allow(clippy::too_many_lines)]
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
        // Defense in depth: even if a row was marked committed by corruption
        // or an older path, a v6 local-contract dependency cannot advance the
        // cursor without the same exact bridge proof.
        let local_dependencies = {
            let mut statement = transaction.prepare(
                "SELECT dependency.operation_id, dependency.committed_evidence_id, dependency.dependency_kind
                   FROM change_batch_mutations AS dependency
                   JOIN local_execution_contracts AS contract
                     ON contract.operation_id = dependency.operation_id
                  WHERE dependency.batch_id = ?1",
            )?;
            let rows = statement
                .query_map([batch_id.to_string()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        for (operation, evidence, kind) in local_dependencies {
            let operation = parse_uuid(&operation)?;
            let evidence = parse_uuid(&evidence)?;
            if !r3_5_cursor_proof_is_exact(
                &self.execution_journal,
                &transaction,
                operation,
                evidence,
                &kind,
            )? {
                return Err(Error::LocalMutationIncomplete);
            }
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
            let next_version = version.checked_add(1).ok_or(Error::InvalidSchema)?;
            let evidence =
                interrupted_mutation_evidence(operation_id, attempt, version, occurred_at);
            insert_mutation_evidence(&transaction, &evidence)?;
            let next = MutationState {
                operation_id,
                phase: MutationPhase::NeedsReconcile,
                attempt_number: attempt,
                state_version: next_version,
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
                next_version,
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

/// Shared proof for every persisted final outcome and every R3.5 bridge.  The
/// journal decoder accepts only canonical encodings, so equality of these
/// typed witnesses is also equality of their sealed canonical bytes.
#[allow(clippy::too_many_arguments)]
fn exact_journal_pair(
    journal: &SyncExecutionJournal,
    expected_pre: &PreSideEffectWitness,
    outcome_id: Uuid,
    evidence_id: Uuid,
    outcome: LocalExecutionOutcome,
    evidence_fingerprint: [u8; 32],
    created_at_unix_ms: u64,
    expected_r3_evidence_fingerprint: JournalR3Expectation,
) -> Result<OutcomeWitness> {
    let pre = journal
        .read_pre(expected_pre.operation_id, expected_pre.attempt_number)?
        .ok_or(Error::LocalExecutionJournalMismatch)?;
    SyncStore::validate_journal_witness_contract(&pre, expected_pre)?;
    if pre != *expected_pre {
        return Err(Error::LocalExecutionJournalMismatch);
    }
    let witness = journal
        .read_outcome(expected_pre.operation_id, expected_pre.attempt_number)?
        .ok_or(Error::LocalExecutionJournalMismatch)?;
    SyncStore::validate_journal_witness_contract(&witness.pre, expected_pre)?;
    if witness.pre != pre
        || witness.outcome_id != outcome_id
        || witness.evidence_id != evidence_id
        || witness.outcome != outcome
        || witness.evidence_fingerprint != evidence_fingerprint
        || witness.created_at_unix_ms != created_at_unix_ms
        || match expected_r3_evidence_fingerprint {
            JournalR3Expectation::GenericUnbridged => {
                witness.r3_mutation_evidence_fingerprint.is_some()
            }
            JournalR3Expectation::AuthoritativePreReceipt(expected)
            | JournalR3Expectation::Bridged(expected) => {
                witness.r3_mutation_evidence_fingerprint != Some(expected)
            }
        }
    {
        return Err(Error::LocalExecutionJournalMismatch);
    }
    Ok(witness)
}

fn bridge_receipt_fingerprint(receipt: &BridgeReceiptFacts) -> [u8; 32] {
    fn field(material: &mut Vec<u8>, tag: &[u8], value: &[u8]) {
        material.extend_from_slice(&(tag.len() as u64).to_be_bytes());
        material.extend_from_slice(tag);
        material.extend_from_slice(&(value.len() as u64).to_be_bytes());
        material.extend_from_slice(value);
    }
    let mut material = Vec::new();
    field(&mut material, b"domain", b"myvault-r3.5-bridge-receipt-v1");
    field(&mut material, b"operation", receipt.operation_id.as_bytes());
    field(
        &mut material,
        b"attempt",
        &receipt.attempt_number.to_be_bytes(),
    );
    field(&mut material, b"boundary", receipt.boundary_id.as_bytes());
    field(
        &mut material,
        b"boundary_time",
        &receipt.boundary_occurred_at_unix_ms.to_be_bytes(),
    );
    field(&mut material, b"contract", &receipt.contract_fingerprint);
    field(&mut material, b"outcome", receipt.outcome_id.as_bytes());
    field(&mut material, b"evidence", receipt.evidence_id.as_bytes());
    field(
        &mut material,
        b"local_evidence",
        &receipt.local_evidence_fingerprint,
    );
    field(
        &mut material,
        b"outcome_time",
        &receipt.outcome_occurred_at_unix_ms.to_be_bytes(),
    );
    field(
        &mut material,
        b"r3_intent",
        receipt.r3_intent_fingerprint.as_bytes(),
    );
    field(
        &mut material,
        b"r3_evidence",
        receipt.r3_evidence_fingerprint.as_bytes(),
    );
    // Avoid an ambiguous `NULL`/empty-string representation in this sealed
    // preimage.  The code itself remains redacted and is validated wherever
    // it is read from SQLite.
    match &receipt.r3_outcome_code {
        Some(code) => {
            field(&mut material, b"r3_outcome_code_present", &[1]);
            field(&mut material, b"r3_outcome_code", code.as_bytes());
        }
        None => field(&mut material, b"r3_outcome_code_present", &[0]),
    }
    field(
        &mut material,
        b"dependency",
        receipt.dependency_kind.as_bytes(),
    );
    field(
        &mut material,
        b"r3_state_phase",
        receipt.r3_state_phase.as_bytes(),
    );
    field(
        &mut material,
        b"r3_state_disposition",
        receipt.r3_state_disposition.as_bytes(),
    );
    field(
        &mut material,
        b"r3_attempt",
        &receipt.r3_attempt_number.to_be_bytes(),
    );
    field(
        &mut material,
        b"state_version",
        &receipt.r3_state_version.to_be_bytes(),
    );
    field(
        &mut material,
        b"r3_last_evidence",
        receipt.r3_last_evidence_id.as_bytes(),
    );
    field(
        &mut material,
        b"event_version",
        &receipt.r3_event_state_version.to_be_bytes(),
    );
    Sha256::digest(material).into()
}

fn bridge_receipt_id(fingerprint: [u8; 32]) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, &fingerprint)
}

fn bridge_consumption_anchor_fingerprint(receipt: &BridgeReceiptFacts) -> [u8; 32] {
    let receipt_fingerprint = bridge_receipt_fingerprint(receipt);
    let mut digest = Sha256::new();
    // Keep this independent from the receipt ID derivation and bind the exact
    // receipt preimage plus the cursor-critical identity fields.
    for field in [
        b"myvault-r3.5-consumption-anchor-v1".as_slice(),
        receipt_fingerprint.as_slice(),
        receipt.operation_id.as_bytes(),
        receipt.attempt_number.to_be_bytes().as_slice(),
        receipt.outcome_id.as_bytes(),
        receipt.evidence_id.as_bytes(),
        receipt.r3_evidence_fingerprint.as_bytes(),
        receipt.dependency_kind.as_bytes(),
    ] {
        append_canonical_bytes(&mut digest, field);
    }
    digest.finalize().into()
}

fn bridge_consumption_anchor_id(fingerprint: [u8; 32]) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, &fingerprint)
}

fn bridge_dependency_kind_code(kind: &str) -> Result<u8> {
    match kind {
        "mutation" => Ok(1),
        "merge_publication" => Ok(2),
        "conflict_copy_publication" => Ok(3),
        "base_publication" => Ok(4),
        _ => Err(Error::LocalExecutionJournalMismatch),
    }
}

/// Creates the exact journal proof published after the receipt transaction.
/// Keep this construction adjacent to the receipt preimage so a new receipt
/// field cannot accidentally become a cursor-authority split view.
fn bridge_consumption_witness(
    pre: &PreSideEffectWitness,
    receipt: &BridgeReceiptFacts,
) -> Result<BridgeConsumptionWitness> {
    let r3_intent_fingerprint = parse_canonical_sha256(&receipt.r3_intent_fingerprint)
        .ok_or(Error::LocalExecutionJournalMismatch)?;
    let r3_evidence_fingerprint = parse_canonical_sha256(&receipt.r3_evidence_fingerprint)
        .ok_or(Error::LocalExecutionJournalMismatch)?;
    let receipt_fingerprint = bridge_receipt_fingerprint(receipt);
    Ok(BridgeConsumptionWitness {
        pre: pre.clone(),
        receipt_id: bridge_receipt_id(receipt_fingerprint),
        receipt_fingerprint,
        outcome_id: receipt.outcome_id,
        evidence_id: receipt.evidence_id,
        local_evidence_fingerprint: receipt.local_evidence_fingerprint,
        outcome_occurred_at_unix_ms: receipt.outcome_occurred_at_unix_ms,
        r3_intent_fingerprint,
        r3_evidence_fingerprint,
        dependency_kind: bridge_dependency_kind_code(&receipt.dependency_kind)?,
    })
}

fn exact_bridge_consumption(
    journal: &SyncExecutionJournal,
    expected: &BridgeConsumptionWitness,
) -> Result<bool> {
    Ok(journal
        .bridge_consumption_is_confirmed(expected.pre.operation_id, expected.pre.attempt_number)?
        && journal
            .read_bridge_consumption(expected.pre.operation_id, expected.pre.attempt_number)?
            .is_some_and(|actual| actual == *expected))
}

fn insert_or_require_exact_bridge_receipt(
    transaction: &Transaction<'_>,
    receipt: &BridgeReceiptFacts,
) -> Result<()> {
    let fingerprint = bridge_receipt_fingerprint(receipt);
    let receipt_id = bridge_receipt_id(fingerprint);
    transaction.execute(
        "INSERT INTO local_execution_r3_bridge_receipts(
            receipt_id, receipt_fingerprint, operation_id, attempt_number, boundary_id,
            boundary_occurred_at_unix_ms, contract_fingerprint, outcome_id, evidence_id,
            local_evidence_fingerprint, outcome_occurred_at_unix_ms, r3_intent_fingerprint,
            r3_evidence_fingerprint, r3_outcome_code, dependency_kind, r3_state_phase, r3_state_disposition,
            r3_attempt_number, r3_state_version, r3_last_evidence_id, r3_event_state_version
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                   ?15, ?16, ?17, ?18, ?19, ?20, ?21)
         ON CONFLICT(receipt_id) DO NOTHING",
        params![
            receipt_id.to_string(),
            fingerprint.as_slice(),
            receipt.operation_id.to_string(),
            i64::from(receipt.attempt_number),
            receipt.boundary_id.to_string(),
            u64_to_i64(receipt.boundary_occurred_at_unix_ms)?,
            receipt.contract_fingerprint.as_slice(),
            receipt.outcome_id.to_string(),
            receipt.evidence_id.to_string(),
            receipt.local_evidence_fingerprint.as_slice(),
            u64_to_i64(receipt.outcome_occurred_at_unix_ms)?,
            receipt.r3_intent_fingerprint,
            receipt.r3_evidence_fingerprint,
            receipt.r3_outcome_code,
            receipt.dependency_kind,
            receipt.r3_state_phase,
            receipt.r3_state_disposition,
            i64::from(receipt.r3_attempt_number),
            u64_to_i64(receipt.r3_state_version)?,
            receipt.r3_last_evidence_id.to_string(),
            u64_to_i64(receipt.r3_event_state_version)?,
        ],
    )?;
    let exact: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_execution_r3_bridge_receipts
          WHERE receipt_id = ?1 AND receipt_fingerprint = ?2 AND operation_id = ?3
            AND attempt_number = ?4 AND boundary_id = ?5 AND boundary_occurred_at_unix_ms = ?6
            AND contract_fingerprint = ?7 AND outcome_id = ?8 AND evidence_id = ?9
            AND local_evidence_fingerprint = ?10 AND outcome_occurred_at_unix_ms = ?11
            AND r3_intent_fingerprint = ?12 AND r3_evidence_fingerprint = ?13
            AND r3_outcome_code IS ?14 AND dependency_kind = ?15 AND r3_state_phase = ?16
            AND r3_state_disposition = ?17 AND r3_attempt_number = ?18
            AND r3_state_version = ?19 AND r3_last_evidence_id = ?20 AND r3_event_state_version = ?21)",
        params![
            receipt_id.to_string(), fingerprint.as_slice(), receipt.operation_id.to_string(),
            i64::from(receipt.attempt_number), receipt.boundary_id.to_string(),
            u64_to_i64(receipt.boundary_occurred_at_unix_ms)?, receipt.contract_fingerprint.as_slice(),
            receipt.outcome_id.to_string(), receipt.evidence_id.to_string(),
            receipt.local_evidence_fingerprint.as_slice(), u64_to_i64(receipt.outcome_occurred_at_unix_ms)?,
            receipt.r3_intent_fingerprint, receipt.r3_evidence_fingerprint, receipt.r3_outcome_code, receipt.dependency_kind,
            receipt.r3_state_phase, receipt.r3_state_disposition, i64::from(receipt.r3_attempt_number),
            u64_to_i64(receipt.r3_state_version)?, receipt.r3_last_evidence_id.to_string(),
            u64_to_i64(receipt.r3_event_state_version)?,
        ],
        |row| row.get(0),
    )?;
    if !exact {
        return Err(Error::LocalExecutionJournalMismatch);
    }
    Ok(())
}

fn insert_or_require_exact_consumption_anchor(
    transaction: &Transaction<'_>,
    receipt: &BridgeReceiptFacts,
) -> Result<()> {
    let receipt_fingerprint = bridge_receipt_fingerprint(receipt);
    let receipt_id = bridge_receipt_id(receipt_fingerprint);
    let anchor_fingerprint = bridge_consumption_anchor_fingerprint(receipt);
    let anchor_id = bridge_consumption_anchor_id(anchor_fingerprint);
    transaction.execute(
        "INSERT INTO local_execution_r3_consumption_anchors(
            anchor_id, anchor_fingerprint, receipt_id, receipt_fingerprint,
            operation_id, attempt_number, outcome_id, evidence_id,
            r3_evidence_fingerprint, dependency_kind
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(anchor_id) DO NOTHING",
        params![
            anchor_id.to_string(),
            anchor_fingerprint.as_slice(),
            receipt_id.to_string(),
            receipt_fingerprint.as_slice(),
            receipt.operation_id.to_string(),
            i64::from(receipt.attempt_number),
            receipt.outcome_id.to_string(),
            receipt.evidence_id.to_string(),
            receipt.r3_evidence_fingerprint,
            receipt.dependency_kind
        ],
    )?;
    let exact: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_execution_r3_consumption_anchors
          WHERE anchor_id = ?1 AND anchor_fingerprint = ?2 AND receipt_id = ?3
            AND receipt_fingerprint = ?4 AND operation_id = ?5 AND attempt_number = ?6
            AND outcome_id = ?7 AND evidence_id = ?8 AND r3_evidence_fingerprint = ?9
            AND dependency_kind = ?10)",
        params![
            anchor_id.to_string(),
            anchor_fingerprint.as_slice(),
            receipt_id.to_string(),
            receipt_fingerprint.as_slice(),
            receipt.operation_id.to_string(),
            i64::from(receipt.attempt_number),
            receipt.outcome_id.to_string(),
            receipt.evidence_id.to_string(),
            receipt.r3_evidence_fingerprint,
            receipt.dependency_kind
        ],
        |row| row.get(0),
    )?;
    if !exact {
        return Err(Error::LocalExecutionJournalMismatch);
    }
    Ok(())
}

fn consumption_anchor_is_exact(
    connection: &Connection,
    receipt: &BridgeReceiptFacts,
) -> Result<bool> {
    let receipt_fingerprint = bridge_receipt_fingerprint(receipt);
    let anchor_fingerprint = bridge_consumption_anchor_fingerprint(receipt);
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM local_execution_r3_consumption_anchors
          WHERE anchor_id = ?1 AND anchor_fingerprint = ?2 AND receipt_id = ?3
            AND receipt_fingerprint = ?4 AND operation_id = ?5 AND attempt_number = ?6
            AND outcome_id = ?7 AND evidence_id = ?8 AND r3_evidence_fingerprint = ?9
            AND dependency_kind = ?10)",
            params![
                bridge_consumption_anchor_id(anchor_fingerprint).to_string(),
                anchor_fingerprint.as_slice(),
                bridge_receipt_id(receipt_fingerprint).to_string(),
                receipt_fingerprint.as_slice(),
                receipt.operation_id.to_string(),
                i64::from(receipt.attempt_number),
                receipt.outcome_id.to_string(),
                receipt.evidence_id.to_string(),
                receipt.r3_evidence_fingerprint,
                receipt.dependency_kind
            ],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

/// Read-only validation used by the final cursor gate. The public commit path
/// has no bypass for this proof: it revalidates the immutable bridge receipt
/// against the current local and R3 rows in the same cursor transaction.
#[allow(clippy::too_many_lines)]
fn r3_5_cursor_proof_is_exact(
    journal: &SyncExecutionJournal,
    transaction: &Transaction<'_>,
    operation_id: Uuid,
    evidence_id: Uuid,
    dependency_kind: &str,
) -> Result<bool> {
    let row: Option<R3_5CursorProofRow> = transaction.query_row(
        "SELECT receipt.receipt_id, receipt.receipt_fingerprint, receipt.boundary_id,
                receipt.boundary_occurred_at_unix_ms, receipt.outcome_id,
                receipt.outcome_occurred_at_unix_ms, receipt.local_evidence_fingerprint,
                receipt.r3_intent_fingerprint, receipt.r3_evidence_fingerprint,
                contract.contract_fingerprint, contract.intent_fingerprint, outcome.attempt_number,
                outcome.outcome_id, outcome.evidence_id, outcome.outcome, evidence.attempt_number,
                state.state_version, state.phase, state.disposition, state.last_evidence_id,
                receipt.dependency_kind, receipt.r3_state_phase, receipt.r3_state_disposition,
                receipt.r3_last_evidence_id, receipt.r3_event_state_version
           FROM local_execution_contracts AS contract
           JOIN local_execution_attempt_outcomes AS outcome ON outcome.operation_id = contract.operation_id
           JOIN local_execution_attempt_boundaries AS boundary
             ON boundary.operation_id = outcome.operation_id AND boundary.attempt_number = outcome.attempt_number
           JOIN local_execution_r3_bridge_receipts AS receipt
             ON receipt.operation_id = outcome.operation_id AND receipt.attempt_number = outcome.attempt_number
           JOIN mutation_intents AS intent ON intent.operation_id = contract.operation_id
           JOIN mutation_verification_evidence AS evidence
             ON evidence.evidence_id = ?2 AND evidence.operation_id = contract.operation_id
           JOIN mutation_state AS state ON state.operation_id = contract.operation_id
           JOIN mutation_events AS event ON event.operation_id = state.operation_id
             AND event.attempt_number = state.attempt_number
             AND event.state_version = state.state_version
             AND event.evidence_id = state.last_evidence_id
          WHERE contract.operation_id = ?1
            AND outcome.evidence_id = ?2
            AND outcome.outcome = 'VerifiedApplied'
            AND outcome.contract_fingerprint = contract.contract_fingerprint
            AND boundary.contract_fingerprint = contract.contract_fingerprint
            AND receipt.boundary_id = boundary.boundary_id
            AND receipt.boundary_occurred_at_unix_ms = boundary.occurred_at_unix_ms
            AND receipt.contract_fingerprint = contract.contract_fingerprint
            AND receipt.outcome_id = outcome.outcome_id AND receipt.evidence_id = outcome.evidence_id
            AND receipt.local_evidence_fingerprint = outcome.evidence_fingerprint
            AND receipt.outcome_occurred_at_unix_ms = outcome.occurred_at_unix_ms
            AND receipt.r3_intent_fingerprint = intent.intent_fingerprint
            AND receipt.r3_evidence_fingerprint = evidence.evidence_fingerprint
            AND receipt.r3_outcome_code IS evidence.outcome_code
            AND evidence.outcome_code IS state.outcome_code
            AND state.outcome_code IS event.outcome_code
            AND receipt.r3_attempt_number = evidence.attempt_number
            AND receipt.r3_attempt_number = state.attempt_number
            AND receipt.r3_state_version = state.state_version
            AND receipt.r3_event_state_version = state.state_version
            AND receipt.r3_last_evidence_id = state.last_evidence_id
            AND receipt.r3_state_phase = state.phase
            AND receipt.r3_state_disposition = state.disposition
            AND receipt.dependency_kind = ?3
            AND evidence.capture_phase = 'post_verify'
            AND evidence.disposition = 'verified_applied'
            AND evidence.forbidden_side_effect = 0
            AND state.phase = 'completed'
            AND state.disposition = 'verified_applied'
            AND event.phase = 'completed'
            AND event.disposition = 'verified_applied'
            AND event.evidence_id = ?2",
        params![operation_id.to_string(), evidence_id.to_string(), dependency_kind],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?, row.get(11)?, row.get(12)?, row.get(13)?, row.get(14)?, row.get(15)?, row.get(16)?, row.get(17)?, row.get(18)?, row.get(19)?, row.get(20)?, row.get(21)?, row.get(22)?, row.get(23)?, row.get(24)?)),
    ).optional()?;
    let Some((
        receipt_id,
        receipt_fingerprint,
        boundary_id,
        boundary_at,
        outcome_id,
        outcome_at,
        local_evidence_fingerprint,
        r3_intent,
        r3_evidence,
        contract,
        contract_intent,
        local_attempt,
        local_outcome_id,
        local_evidence,
        local_outcome,
        r3_attempt,
        state_version,
        state_phase,
        state_disposition,
        last_evidence,
        dependency_kind,
        receipt_state_phase,
        receipt_state_disposition,
        receipt_last_evidence,
        event_version,
    )) = row
    else {
        return Ok(false);
    };
    let Ok(boundary_id) = parse_uuid(&boundary_id) else {
        return Ok(false);
    };
    let Ok(outcome_id) = parse_uuid(&outcome_id) else {
        return Ok(false);
    };
    let Ok(local_outcome_id) = parse_uuid(&local_outcome_id) else {
        return Ok(false);
    };
    let Ok(local_evidence_id) = parse_uuid(&local_evidence) else {
        return Ok(false);
    };
    let Ok(contract) = blob32(contract) else {
        return Ok(false);
    };
    let Ok(contract_intent) = blob32(contract_intent) else {
        return Ok(false);
    };
    let Ok(local_evidence_fingerprint) = blob32(local_evidence_fingerprint) else {
        return Ok(false);
    };
    let Ok(receipt_fingerprint) = blob32(receipt_fingerprint) else {
        return Ok(false);
    };
    let Ok(boundary_at) = u64::try_from(boundary_at) else {
        return Ok(false);
    };
    let Ok(outcome_at) = u64::try_from(outcome_at) else {
        return Ok(false);
    };
    let Ok(r3_attempt) = u32::try_from(r3_attempt) else {
        return Ok(false);
    };
    let Ok(state_version) = u64::try_from(state_version) else {
        return Ok(false);
    };
    let Ok(event_version) = u64::try_from(event_version) else {
        return Ok(false);
    };
    let Ok(local_attempt) = u32::try_from(local_attempt) else {
        return Ok(false);
    };
    let Ok(local_outcome) = LocalExecutionOutcome::parse(&local_outcome) else {
        return Ok(false);
    };
    let Ok(receipt_last_evidence) = parse_uuid(&receipt_last_evidence) else {
        return Ok(false);
    };
    let r3_outcome_code: Option<String> = transaction.query_row(
        "SELECT outcome_code FROM mutation_state WHERE operation_id = ?1",
        [operation_id.to_string()],
        |row| row.get(0),
    )?;
    if r3_outcome_code
        .as_deref()
        .is_some_and(|code| validate_redacted_code(code).is_err())
    {
        return Ok(false);
    }
    if !persisted_verified_applied_mutation_evidence_is_exact(transaction, evidence_id)? {
        return Ok(false);
    }
    if !mutation_history_is_exact(transaction, operation_id)? {
        return Ok(false);
    }
    // The sealed journal is an immutable witness, not authority.  Requiring
    // this exact independent preimage prevents a coherent rewrite of the
    // SQLite outcome/receipt circular hashes from becoming cursor authority.
    let collision: [u8; 32] = match transaction.query_row(
        "SELECT collision_snapshot_fingerprint FROM local_execution_contracts WHERE operation_id = ?1",
        [operation_id.to_string()],
        |row| blob32(row.get(0)?).map_err(to_sql_error),
    ) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let expected_pre = PreSideEffectWitness {
        operation_id,
        attempt_number: local_attempt,
        boundary_id,
        boundary_occurred_at_unix_ms: boundary_at,
        intent_fingerprint: contract_intent,
        contract_fingerprint: contract,
        collision_snapshot_fingerprint: collision,
        created_at_unix_ms: boundary_at,
    };
    if exact_journal_pair(
        journal,
        &expected_pre,
        outcome_id,
        local_evidence_id,
        local_outcome,
        local_evidence_fingerprint,
        outcome_at,
        match parse_canonical_sha256(&r3_evidence) {
            Some(value) => JournalR3Expectation::Bridged(value),
            None => return Ok(false),
        },
    )
    .is_err()
    {
        return Ok(false);
    }
    let (event_count, distinct_event_versions, minimum_event_version, maximum_event_version): (
        i64,
        i64,
        Option<i64>,
        Option<i64>,
    ) = transaction.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT state_version), MIN(state_version), MAX(state_version)
           FROM mutation_events WHERE operation_id = ?1 AND state_version BETWEEN 0 AND ?2",
        params![operation_id.to_string(), u64_to_i64(state_version)?],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let Some(expected_event_count) = u64_to_i64(state_version)?.checked_add(1) else {
        return Ok(false);
    };
    // Count all operation events: an extra future version is a forged
    // history, even though it lies outside the state frontier.
    let all_event_count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM mutation_events WHERE operation_id = ?1",
        [operation_id.to_string()],
        |row| row.get(0),
    )?;
    if event_count != expected_event_count
        || all_event_count != event_count
        || distinct_event_versions != event_count
        || minimum_event_version != Some(0)
        || maximum_event_version != Some(u64_to_i64(state_version)?)
    {
        return Ok(false);
    }
    let Ok(expected_local_intent) = local_intent_fingerprint_from_r3_intent(&r3_intent) else {
        return Ok(false);
    };
    let receipt = BridgeReceiptFacts {
        operation_id,
        attempt_number: local_attempt,
        boundary_id,
        boundary_occurred_at_unix_ms: boundary_at,
        contract_fingerprint: contract,
        outcome_id,
        evidence_id: local_evidence_id,
        local_evidence_fingerprint,
        outcome_occurred_at_unix_ms: outcome_at,
        r3_intent_fingerprint: r3_intent,
        r3_evidence_fingerprint: r3_evidence,
        r3_outcome_code,
        dependency_kind,
        r3_state_phase: receipt_state_phase,
        r3_state_disposition: receipt_state_disposition,
        r3_attempt_number: r3_attempt,
        r3_state_version: state_version,
        r3_last_evidence_id: receipt_last_evidence,
        r3_event_state_version: event_version,
    };
    let Ok(expected_consumption) = bridge_consumption_witness(&expected_pre, &receipt) else {
        return Ok(false);
    };
    if parse_uuid(&receipt_id).ok() != Some(bridge_receipt_id(receipt_fingerprint))
        || receipt_fingerprint != bridge_receipt_fingerprint(&receipt)
        || !consumption_anchor_is_exact(transaction, &receipt)?
        || outcome_id != local_outcome_id
        || receipt.evidence_id != evidence_id
        || contract_intent != expected_local_intent
        || authoritative_outcome_id(
            operation_id,
            local_attempt,
            boundary_id,
            boundary_at,
            local_evidence_id,
            local_evidence_fingerprint,
            local_outcome,
            outcome_at,
        ) != outcome_id
        || state_phase != "completed"
        || state_disposition != "verified_applied"
        || last_evidence != evidence_id.to_string()
        || !exact_bridge_consumption(journal, &expected_consumption)?
    {
        return Ok(false);
    }
    Ok(true)
}

#[allow(dead_code)] // Retained as the narrower generic reopen primitive.
fn persisted_mutation_evidence_fingerprint_is_exact(
    connection: &Connection,
    evidence_id: Uuid,
) -> Result<bool> {
    let evidence: Option<MutationVerificationEvidence> = connection.query_row(
        "SELECT evidence_id, operation_id, attempt_number, capture_phase, disposition, outcome_code,
                observed_account_id, observed_remote_root_id, observed_remote_file_id, observed_parent_id,
                observed_path, observed_local_revision, observed_remote_revision, observed_sha256,
                observed_byte_length, observed_operation_marker, forbidden_side_effect,
                verified_received_byte_offset, resume_reference, evidence_fingerprint, captured_at_unix_ms
           FROM mutation_verification_evidence WHERE evidence_id = ?1",
        [evidence_id.to_string()],
        |row| {
            Ok(MutationVerificationEvidence {
                evidence_id: parse_uuid(&row.get::<_, String>(0)?).map_err(to_sql_error)?,
                operation_id: parse_uuid(&row.get::<_, String>(1)?).map_err(to_sql_error)?,
                attempt_number: u32::try_from(row.get::<_, i64>(2)?)
                    .map_err(|_| to_sql_error(Error::InvalidSchema))?,
                capture_phase: match row.get::<_, String>(3)?.as_str() {
                    "preflight" => MutationEvidenceCapturePhase::Preflight,
                    "post_verify" => MutationEvidenceCapturePhase::PostVerify,
                    "reconcile" => MutationEvidenceCapturePhase::Reconcile,
                    _ => return Err(to_sql_error(Error::InvalidSchema)),
                },
                disposition: MutationDisposition::parse(&row.get::<_, String>(4)?)
                    .map_err(to_sql_error)?,
                outcome_code: row.get(5)?, observed_account_id: row.get(6)?,
                observed_remote_root_id: row.get(7)?, observed_remote_file_id: row.get(8)?,
                observed_parent_id: row.get(9)?, observed_path: row.get(10)?,
                observed_local_revision: row.get(11)?, observed_remote_revision: row.get(12)?,
                observed_sha256: row.get(13)?,
                observed_byte_length: row.get::<_, Option<i64>>(14)?.map(u64::try_from).transpose()
                    .map_err(|_| to_sql_error(Error::InvalidSchema))?,
                observed_operation_marker: row.get(15)?,
                forbidden_side_effect: row.get::<_, i64>(16)? == 1,
                verified_received_byte_offset: row.get::<_, Option<i64>>(17)?.map(u64::try_from).transpose()
                    .map_err(|_| to_sql_error(Error::InvalidSchema))?,
                resume_reference: row.get(18)?, evidence_fingerprint: row.get(19)?,
                captured_at_unix_ms: u64::try_from(row.get::<_, i64>(20)?)
                    .map_err(|_| to_sql_error(Error::InvalidSchema))?,
            })
        },
    ).optional()?;
    Ok(evidence.is_some_and(|value| {
        value.evidence_id == evidence_id
            && value.evidence_fingerprint == value.canonical_fingerprint()
    }))
}

/// Reopens the exact persisted R3 post-verify evidence as a typed value and
/// validates both its canonical preimage and its semantic relationship to the
/// immutable intent.  A self-consistent fingerprint alone is not evidence of
/// a valid verified-applied observation.
fn persisted_verified_applied_mutation_evidence_is_exact(
    connection: &Connection,
    evidence_id: Uuid,
) -> Result<bool> {
    let evidence: Option<MutationVerificationEvidence> = connection.query_row(
        "SELECT evidence_id, operation_id, attempt_number, capture_phase, disposition, outcome_code,
                observed_account_id, observed_remote_root_id, observed_remote_file_id, observed_parent_id,
                observed_path, observed_local_revision, observed_remote_revision, observed_sha256,
                observed_byte_length, observed_operation_marker, forbidden_side_effect,
                verified_received_byte_offset, resume_reference, evidence_fingerprint, captured_at_unix_ms
           FROM mutation_verification_evidence WHERE evidence_id = ?1",
        [evidence_id.to_string()],
        mutation_verification_evidence_from_row,
    ).optional()?;
    Ok(evidence.is_some_and(|value| {
        value.evidence_id == evidence_id
            && value.capture_phase == MutationEvidenceCapturePhase::PostVerify
            && value.disposition == MutationDisposition::VerifiedApplied
            && !value.forbidden_side_effect
            && validate_mutation_evidence(&value).is_ok()
            && validate_verified_applied_evidence(connection, &value).is_ok()
    }))
}

/// Validates the whole append-only R3 history, not only the event that happens
/// to match the current state.  R3.1/R3.5 retain a fail-closed no-blind-retry
/// policy: a retry can occur only after exact persisted retry evidence and a
/// due-time claim; the new Running attempt is not replay authority by itself.
/// Supported histories additionally include
/// `Running -> RetryScheduled -> Running(next attempt)`.
#[allow(clippy::too_many_lines)]
fn mutation_history_is_exact(connection: &Connection, operation_id: Uuid) -> Result<bool> {
    let Some(intent) = load_persisted_mutation_intent(connection, operation_id)? else {
        return Ok(false);
    };
    if intent.operation_id != operation_id || validate_mutation_intent(&intent).is_err() {
        return Ok(false);
    }
    let Some(state) = load_mutation_state(connection, operation_id)? else {
        return Ok(false);
    };
    let mut statement = connection.prepare(
        "SELECT event_id, operation_id, attempt_number, state_version, phase, disposition,
                evidence_id, outcome_code, occurred_at_unix_ms
           FROM mutation_events WHERE operation_id = ?1 ORDER BY state_version, event_id",
    )?;
    let events = statement
        .query_map([operation_id.to_string()], |row| {
            row_to_mutation_event(row).map_err(to_sql_error)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if state
        .state_version
        .checked_add(1)
        .and_then(|n| usize::try_from(n).ok())
        != Some(events.len())
    {
        return Ok(false);
    }
    let Some(initial) = events.first() else {
        return Ok(false);
    };
    if initial.operation_id != operation_id
        || initial.state_version != 0
        || initial.attempt_number != 0
        || initial.phase != MutationPhase::IntentDurable
        || initial.disposition.is_some()
        || initial.evidence_id.is_some()
        || initial.outcome_code.is_some()
        || initial.occurred_at_unix_ms != intent.registered_at_unix_ms
    {
        return Ok(false);
    }
    let mut previous = initial;
    for (version, event) in events.iter().enumerate().skip(1) {
        let retry_claim = previous.phase == MutationPhase::RetryScheduled
            && event.phase == MutationPhase::Running;
        let expected_attempt = previous.attempt_number.checked_add(u32::from(retry_claim));
        if event.operation_id != operation_id
            || event.state_version != u64::try_from(version).map_err(|_| Error::InvalidSchema)?
            || Some(event.attempt_number) != expected_attempt
            || event.occurred_at_unix_ms < previous.occurred_at_unix_ms
        {
            return Ok(false);
        }
        let legal = matches!(
            (previous.phase, event.phase),
            (
                MutationPhase::IntentDurable,
                MutationPhase::Running | MutationPhase::NeedsReconcile
            ) | (
                MutationPhase::Running | MutationPhase::NeedsReconcile,
                MutationPhase::Completed
            ) | (
                MutationPhase::Running,
                MutationPhase::NeedsReconcile | MutationPhase::RetryScheduled
            ) | (MutationPhase::RetryScheduled, MutationPhase::Running)
        );
        if !legal {
            return Ok(false);
        }
        match event.phase {
            MutationPhase::Running => {
                if event.disposition.is_some()
                    || event.evidence_id.is_some()
                    || event.outcome_code.is_some()
                {
                    return Ok(false);
                }
            }
            MutationPhase::Completed
            | MutationPhase::NeedsReconcile
            | MutationPhase::RetryScheduled => {
                let Some(evidence_id) = event.evidence_id else {
                    return Ok(false);
                };
                let Some(evidence) = connection.query_row(
                    "SELECT evidence_id, operation_id, attempt_number, capture_phase, disposition, outcome_code,
                            observed_account_id, observed_remote_root_id, observed_remote_file_id, observed_parent_id,
                            observed_path, observed_local_revision, observed_remote_revision, observed_sha256,
                            observed_byte_length, observed_operation_marker, forbidden_side_effect,
                            verified_received_byte_offset, resume_reference, evidence_fingerprint, captured_at_unix_ms
                       FROM mutation_verification_evidence WHERE evidence_id = ?1",
                    [evidence_id.to_string()], mutation_verification_evidence_from_row,
                ).optional()? else { return Ok(false) };
                if validate_mutation_evidence(&evidence).is_err()
                    || evidence.operation_id != operation_id
                    || evidence.attempt_number != event.attempt_number
                    || evidence.disposition
                        != event.disposition.unwrap_or(MutationDisposition::RetrySafe)
                    || evidence.outcome_code != event.outcome_code
                    || evidence.captured_at_unix_ms != event.occurred_at_unix_ms
                {
                    return Ok(false);
                }
                match event.phase {
                    MutationPhase::Completed
                        if event.disposition == Some(MutationDisposition::VerifiedApplied)
                            && evidence.capture_phase
                                == MutationEvidenceCapturePhase::PostVerify
                            && !evidence.forbidden_side_effect =>
                    {
                        if validate_verified_applied_evidence_against_intent(&intent, &evidence)
                            .is_err()
                        {
                            return Ok(false);
                        }
                    }
                    MutationPhase::NeedsReconcile
                        if event.disposition == Some(MutationDisposition::NeedsReconcile) =>
                    {
                        // Initial reconciliation is not a generic escape hatch:
                        // only the non-executable remote-existing contract may
                        // make this direct transition, and every captured fact
                        // must still equal the immutable intent.
                        if previous.phase == MutationPhase::IntentDurable
                            && validate_remote_existing_blocked_initial_needs_reconcile(
                                &intent, &evidence,
                            )
                            .is_err()
                        {
                            return Ok(false);
                        }
                    }
                    MutationPhase::RetryScheduled
                        if event.disposition == Some(MutationDisposition::VerifiedNotApplied)
                            && evidence.capture_phase
                                == MutationEvidenceCapturePhase::Reconcile
                            && evidence.forbidden_side_effect =>
                    {
                        if validate_verified_applied_evidence_against_intent(&intent, &evidence)
                            .is_err()
                        {
                            return Ok(false);
                        }
                    }
                    MutationPhase::RetryScheduled
                        if event.disposition == Some(MutationDisposition::RetrySafe)
                            && evidence.capture_phase
                                == MutationEvidenceCapturePhase::Reconcile
                            && !evidence.forbidden_side_effect
                            && evidence.resume_reference.is_some()
                            && evidence.verified_received_byte_offset.is_some() =>
                    {
                        if validate_verified_applied_evidence_against_intent(&intent, &evidence)
                            .is_err()
                        {
                            return Ok(false);
                        }
                    }
                    _ => return Ok(false),
                }
                if event.phase == MutationPhase::RetryScheduled
                    && !mutation_retry_contract_matches_event(connection, event)?
                {
                    return Ok(false);
                }
            }
            MutationPhase::IntentDurable => return Ok(false),
        }
        previous = event;
    }
    let retry_state_is_exact = match state.phase {
        MutationPhase::RetryScheduled => match state.disposition {
            Some(MutationDisposition::VerifiedNotApplied) => {
                state.next_attempt_at_unix_ms.is_some()
                    && state.retry_mode == Some(MutationRetryMode::RestartExact)
                    && state.resume_reference.is_none()
            }
            Some(MutationDisposition::RetrySafe) => {
                state.next_attempt_at_unix_ms.is_some()
                    && state.retry_mode == Some(MutationRetryMode::ResumeExact)
                    && state.resume_reference.is_some()
            }
            _ => false,
        },
        _ => {
            state.next_attempt_at_unix_ms.is_none()
                && state.retry_mode.is_none()
                && state.resume_reference.is_none()
        }
    };
    let retry_event_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM mutation_events WHERE operation_id = ?1 AND phase = 'retry_scheduled'",
        [operation_id.to_string()], |row| row.get(0),
    )?;
    let retry_contract_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM mutation_retry_contracts WHERE operation_id = ?1",
        [operation_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(retry_state_is_exact
        && retry_event_count == retry_contract_count
        && (state.phase != MutationPhase::RetryScheduled
            || mutation_retry_contract_matches_state(connection, &state)?)
        && previous.operation_id == state.operation_id
        && previous.attempt_number == state.attempt_number
        && previous.state_version == state.state_version
        && previous.phase == state.phase
        && previous.disposition == state.disposition
        && previous.evidence_id == state.last_evidence_id
        && previous.outcome_code == state.outcome_code
        && previous.occurred_at_unix_ms == state.updated_at_unix_ms)
}

fn mutation_verification_evidence_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<MutationVerificationEvidence> {
    Ok(MutationVerificationEvidence {
        evidence_id: parse_uuid(&row.get::<_, String>(0)?).map_err(to_sql_error)?,
        operation_id: parse_uuid(&row.get::<_, String>(1)?).map_err(to_sql_error)?,
        attempt_number: u32::try_from(row.get::<_, i64>(2)?)
            .map_err(|_| to_sql_error(Error::InvalidSchema))?,
        capture_phase: match row.get::<_, String>(3)?.as_str() {
            "preflight" => MutationEvidenceCapturePhase::Preflight,
            "post_verify" => MutationEvidenceCapturePhase::PostVerify,
            "reconcile" => MutationEvidenceCapturePhase::Reconcile,
            _ => return Err(to_sql_error(Error::InvalidSchema)),
        },
        disposition: MutationDisposition::parse(&row.get::<_, String>(4)?).map_err(to_sql_error)?,
        outcome_code: row.get(5)?,
        observed_account_id: row.get(6)?,
        observed_remote_root_id: row.get(7)?,
        observed_remote_file_id: row.get(8)?,
        observed_parent_id: row.get(9)?,
        observed_path: row.get(10)?,
        observed_local_revision: row.get(11)?,
        observed_remote_revision: row.get(12)?,
        observed_sha256: row.get(13)?,
        observed_byte_length: row
            .get::<_, Option<i64>>(14)?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| to_sql_error(Error::InvalidSchema))?,
        observed_operation_marker: row.get(15)?,
        forbidden_side_effect: row.get::<_, i64>(16)? == 1,
        verified_received_byte_offset: row
            .get::<_, Option<i64>>(17)?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| to_sql_error(Error::InvalidSchema))?,
        resume_reference: row.get(18)?,
        evidence_fingerprint: row.get(19)?,
        captured_at_unix_ms: u64::try_from(row.get::<_, i64>(20)?)
            .map_err(|_| to_sql_error(Error::InvalidSchema))?,
    })
}

fn local_execution_completion_id(operation_id: Uuid) -> String {
    format!("local-execution-completion:{operation_id}")
}

fn insert_local_execution_identity(
    transaction: &Transaction<'_>,
    operation_id: Uuid,
    identity: &PersistedIdentityEvidence<'_>,
) -> Result<()> {
    let evidence_id = format!("local-execution-identity:{operation_id}:{}", identity.role);
    transaction.execute(
        "INSERT INTO local_execution_identity_evidence(
            evidence_id, operation_id, role, evidence_version, evidence_kind,
            object_kind, provider_id, object_id, attestation, stable_identity_fingerprint
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            evidence_id,
            operation_id.to_string(),
            identity.role,
            i64::from(identity.version),
            i64::from(identity.kind),
            i64::from(identity.object_kind),
            identity.provider_id,
            identity.object_id,
            identity.attestation,
            identity.stable_identity_fingerprint.as_slice(),
        ],
    )?;
    Ok(())
}

fn blob32(value: Vec<u8>) -> Result<[u8; 32]> {
    value.try_into().map_err(|_| Error::InvalidSchema)
}

fn parse_canonical_sha256(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut parsed = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(chunk[0])?;
        let low = hex_nibble(chunk[1])?;
        parsed[index] = (high << 4) | low;
    }
    Some(parsed)
}

const fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn to_sql_error(error: Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
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

/// Reconstructs the complete persisted immutable preimage.  Callers that
/// validate evidence must use this value rather than a lossy projection so a
/// `SQLite` rewrite of a field not observed by the evidence cannot create a
/// split view of the operation.
fn load_persisted_mutation_intent(
    connection: &Connection,
    operation_id: Uuid,
) -> Result<Option<MutationIntent>> {
    connection
        .query_row(
            "SELECT operation_id, operation_kind, account_id, remote_root_id, remote_file_id,
                    source_parent_id, destination_parent_id, local_object_id, source_path,
                    destination_path, expected_local_revision, expected_remote_revision,
                    base_reference, base_local_revision, base_remote_revision, base_sha256,
                    base_byte_length, expected_local_sha256, expected_local_byte_length,
                    expected_remote_sha256, expected_remote_byte_length, operation_marker,
                    intent_fingerprint, registered_at_unix_ms
             FROM mutation_intents WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| {
                Ok(MutationIntent {
                    operation_id: parse_uuid(&row.get::<_, String>(0)?).map_err(to_sql_error)?,
                    operation_kind: MutationOperationKind::parse(&row.get::<_, String>(1)?)
                        .map_err(to_sql_error)?,
                    account_id: row.get(2)?,
                    remote_root_id: row.get(3)?,
                    remote_file_id: row.get(4)?,
                    source_parent_id: row.get(5)?,
                    destination_parent_id: row.get(6)?,
                    local_object_id: row.get(7)?,
                    source_path: row.get(8)?,
                    destination_path: row.get(9)?,
                    expected_local_revision: row.get(10)?,
                    expected_remote_revision: row.get(11)?,
                    base_reference: row.get(12)?,
                    base_local_revision: row.get(13)?,
                    base_remote_revision: row.get(14)?,
                    base_sha256: row.get(15)?,
                    base_byte_length: row
                        .get::<_, Option<i64>>(16)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| to_sql_error(Error::InvalidSchema))?,
                    expected_local_sha256: row.get(17)?,
                    expected_local_byte_length: row
                        .get::<_, Option<i64>>(18)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| to_sql_error(Error::InvalidSchema))?,
                    expected_remote_sha256: row.get(19)?,
                    expected_remote_byte_length: row
                        .get::<_, Option<i64>>(20)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| to_sql_error(Error::InvalidSchema))?,
                    operation_marker: row.get(21)?,
                    intent_fingerprint: row.get(22)?,
                    registered_at_unix_ms: u64::try_from(row.get::<_, i64>(23)?)
                        .map_err(|_| to_sql_error(Error::InvalidSchema))?,
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
    let intent = load_persisted_mutation_intent(connection, evidence.operation_id)?
        .ok_or(Error::MutationNotFound)?;
    validate_mutation_intent(&intent)?;
    validate_verified_applied_evidence_against_intent(&intent, evidence)
}

fn validate_verified_applied_evidence_against_intent(
    intent: &MutationIntent,
    evidence: &MutationVerificationEvidence,
) -> Result<()> {
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

/// The one permitted initial `IntentDurable -> NeedsReconcile` representation.
/// This is deliberately shared by registration and history validation so a
/// self-canonical forensic rewrite cannot turn a persisted initial event into
/// a different blocked contract after registration has accepted it.
fn validate_remote_existing_blocked_initial_needs_reconcile(
    intent: &MutationIntent,
    evidence: &MutationVerificationEvidence,
) -> Result<()> {
    if intent.operation_kind != MutationOperationKind::RemoteExistingBlocked
        || evidence.operation_id != intent.operation_id
        || evidence.attempt_number != 0
        || evidence.capture_phase != MutationEvidenceCapturePhase::Preflight
        || evidence.disposition != MutationDisposition::NeedsReconcile
        || !evidence.forbidden_side_effect
        || evidence.outcome_code.as_deref() != Some("remote_existing_blocked")
        || evidence.captured_at_unix_ms != intent.registered_at_unix_ms
        || evidence.observed_account_id != intent.account_id
        || evidence.observed_remote_root_id != intent.remote_root_id
        || evidence.observed_remote_file_id != intent.remote_file_id
        || evidence.observed_parent_id != intent.source_parent_id
        || evidence.observed_path != intent.source_path
        || evidence.observed_local_revision != intent.expected_local_revision
        || evidence.observed_remote_revision != intent.expected_remote_revision
        || evidence.observed_sha256 != intent.expected_remote_sha256
        || evidence.observed_byte_length != intent.expected_remote_byte_length
        || evidence.observed_operation_marker.as_deref() != Some(intent.operation_marker.as_str())
        || evidence.verified_received_byte_offset.is_some()
        || evidence.resume_reference.is_some()
    {
        return Err(Error::InvalidTransferEvidence);
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
        MutationOutcomeTransition::VerifiedNotApplied {
            next_attempt_at_unix_ms,
        } if current == MutationPhase::Running
            && evidence.disposition == MutationDisposition::VerifiedNotApplied
            && evidence.capture_phase == MutationEvidenceCapturePhase::Reconcile
            && evidence.forbidden_side_effect
            && evidence.outcome_code.as_deref() == Some("verified_not_applied")
            && *next_attempt_at_unix_ms >= evidence.captured_at_unix_ms =>
        {
            Ok((
                MutationPhase::RetryScheduled,
                MutationDisposition::VerifiedNotApplied,
                Some(*next_attempt_at_unix_ms),
                Some(MutationRetryMode::RestartExact),
                None,
            ))
        }
        MutationOutcomeTransition::RetrySafe {
            next_attempt_at_unix_ms,
            resume_reference,
        } if current == MutationPhase::Running
            && evidence.disposition == MutationDisposition::RetrySafe
            && evidence.capture_phase == MutationEvidenceCapturePhase::Reconcile
            && !evidence.forbidden_side_effect
            && evidence.outcome_code.as_deref() == Some("retry_safe")
            && evidence.resume_reference.as_deref() == Some(resume_reference.as_str())
            && evidence.verified_received_byte_offset.is_some()
            && *next_attempt_at_unix_ms >= evidence.captured_at_unix_ms =>
        {
            Ok((
                MutationPhase::RetryScheduled,
                MutationDisposition::RetrySafe,
                Some(*next_attempt_at_unix_ms),
                Some(MutationRetryMode::ResumeExact),
                Some(resume_reference.clone()),
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

fn insert_mutation_retry_contract(
    transaction: &Transaction<'_>,
    state: &MutationState,
    evidence: &MutationVerificationEvidence,
) -> Result<()> {
    let (Some(disposition), Some(due), Some(mode)) = (
        state.disposition,
        state.next_attempt_at_unix_ms,
        state.retry_mode,
    ) else {
        return Err(Error::InvalidStateTransition);
    };
    transaction.execute(
        "INSERT INTO mutation_retry_contracts(
            operation_id, state_version, attempt_number, evidence_id, evidence_fingerprint,
            disposition, outcome_code, due_at_unix_ms, retry_mode, resume_reference,
            verified_received_byte_offset, captured_at_unix_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            state.operation_id.to_string(),
            u64_to_i64(state.state_version)?,
            i64::from(state.attempt_number),
            evidence.evidence_id.to_string(),
            evidence.evidence_fingerprint,
            disposition.as_str(),
            evidence.outcome_code,
            u64_to_i64(due)?,
            mode.as_str(),
            state.resume_reference,
            evidence
                .verified_received_byte_offset
                .map(u64_to_i64)
                .transpose()?,
            u64_to_i64(evidence.captured_at_unix_ms)?
        ],
    )?;
    if !mutation_retry_contract_matches_state(transaction, state)? {
        return Err(Error::LocalMutationIncomplete);
    }
    Ok(())
}

fn mutation_retry_contract_matches_state(
    connection: &Connection,
    state: &MutationState,
) -> Result<bool> {
    if state.phase != MutationPhase::RetryScheduled {
        return Ok(false);
    }
    let Some(evidence_id) = state.last_evidence_id else {
        return Ok(false);
    };
    let Some(disposition) = state.disposition else {
        return Ok(false);
    };
    let Some(due) = state.next_attempt_at_unix_ms else {
        return Ok(false);
    };
    let Some(mode) = state.retry_mode else {
        return Ok(false);
    };
    connection.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM mutation_retry_contracts AS contract
           JOIN mutation_verification_evidence AS evidence ON evidence.evidence_id = contract.evidence_id
            WHERE contract.operation_id = ?1 AND contract.state_version = ?2
              AND contract.attempt_number = ?3 AND contract.evidence_id = ?4
              AND contract.evidence_fingerprint = evidence.evidence_fingerprint
              AND contract.disposition = ?5 AND contract.outcome_code IS ?6
              AND contract.due_at_unix_ms = ?7 AND contract.retry_mode = ?8
              AND contract.resume_reference IS ?9
              AND contract.verified_received_byte_offset IS evidence.verified_received_byte_offset
              AND contract.captured_at_unix_ms = evidence.captured_at_unix_ms
              AND contract.due_at_unix_ms >= evidence.captured_at_unix_ms
              AND evidence.operation_id = ?1 AND evidence.attempt_number = ?3
              AND evidence.disposition = ?5 AND evidence.outcome_code IS ?6
              AND evidence.resume_reference IS ?9)",
        params![state.operation_id.to_string(), u64_to_i64(state.state_version)?,
            i64::from(state.attempt_number), evidence_id.to_string(), disposition.as_str(),
            state.outcome_code, u64_to_i64(due)?, mode.as_str(), state.resume_reference],
        |row| row.get(0),
    ).map_err(Into::into)
}

fn mutation_retry_contract_matches_event(
    connection: &Connection,
    event: &MutationEvent,
) -> Result<bool> {
    let Some(evidence_id) = event.evidence_id else {
        return Ok(false);
    };
    let Some(disposition) = event.disposition else {
        return Ok(false);
    };
    let state = MutationState {
        operation_id: event.operation_id, attempt_number: event.attempt_number,
        state_version: event.state_version, phase: MutationPhase::RetryScheduled,
        disposition: Some(disposition), next_attempt_at_unix_ms: connection.query_row(
            "SELECT due_at_unix_ms FROM mutation_retry_contracts WHERE operation_id = ?1 AND state_version = ?2",
            params![event.operation_id.to_string(), u64_to_i64(event.state_version)?], |row| {
                u64::try_from(row.get::<_, i64>(0)?)
                    .map_err(|_| to_sql_error(Error::InvalidSchema))
            },
        ).optional()?, retry_mode: connection.query_row(
            "SELECT retry_mode FROM mutation_retry_contracts WHERE operation_id = ?1 AND state_version = ?2",
            params![event.operation_id.to_string(), u64_to_i64(event.state_version)?], |row| {
                MutationRetryMode::parse(&row.get::<_, String>(0)?).map_err(to_sql_error)
            },
        ).optional()?, resume_reference: connection.query_row(
            "SELECT resume_reference FROM mutation_retry_contracts WHERE operation_id = ?1 AND state_version = ?2",
            params![event.operation_id.to_string(), u64_to_i64(event.state_version)?], |row| row.get(0),
        ).optional()?.flatten(), last_evidence_id: Some(evidence_id),
        outcome_code: event.outcome_code.clone(), updated_at_unix_ms: event.occurred_at_unix_ms,
    };
    mutation_retry_contract_matches_state(connection, &state)
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

fn row_to_mutation_event(row: &rusqlite::Row<'_>) -> Result<MutationEvent> {
    let outcome_code: Option<String> = row.get(7)?;
    if let Some(code) = &outcome_code {
        validate_redacted_code(code)?;
    }
    Ok(MutationEvent {
        event_id: u64::try_from(row.get::<_, i64>(0)?).map_err(|_| Error::InvalidSchema)?,
        operation_id: parse_uuid(&row.get::<_, String>(1)?)?,
        attempt_number: u32::try_from(row.get::<_, i64>(2)?).map_err(|_| Error::InvalidSchema)?,
        state_version: u64::try_from(row.get::<_, i64>(3)?).map_err(|_| Error::InvalidSchema)?,
        phase: MutationPhase::parse(&row.get::<_, String>(4)?)?,
        disposition: row
            .get::<_, Option<String>>(5)?
            .map(|value| MutationDisposition::parse(&value))
            .transpose()?,
        evidence_id: row
            .get::<_, Option<String>>(6)?
            .map(|value| parse_uuid(&value))
            .transpose()?,
        outcome_code,
        occurred_at_unix_ms: u64::try_from(row.get::<_, i64>(8)?)
            .map_err(|_| Error::InvalidSchema)?,
    })
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

pub(crate) fn create_or_open_storage_dir(
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

pub(crate) fn harden_new_storage_file(
    file: &cap_std::fs::File,
    policy: PrivateStoragePolicy,
) -> Result<()> {
    match policy {
        PrivateStoragePolicy::Standard => private_fs::set_private_file_permissions(file)?,
        #[cfg(target_os = "android")]
        PrivateStoragePolicy::NativeAndroidNoBackup => {
            private_fs::harden_android_new_file(file)?;
        }
    }
    Ok(())
}

pub(crate) fn verify_storage_file(
    file: &cap_std::fs::File,
    policy: PrivateStoragePolicy,
) -> Result<()> {
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
    let _ = open_existing_storage_file(parent, name, policy)?;
    Ok(())
}

pub(crate) fn open_existing_storage_file(
    parent: &Dir,
    name: impl AsRef<Path>,
    policy: PrivateStoragePolicy,
) -> Result<cap_std::fs::File> {
    match policy {
        PrivateStoragePolicy::Standard => Ok(private_fs::open_private_file(parent, name, 1)?),
        #[cfg(target_os = "android")]
        PrivateStoragePolicy::NativeAndroidNoBackup => {
            Ok(private_fs::open_android_private_file(parent, name)?)
        }
    }
}

/// Opens an existing private storage file with the write capability required
/// to flush a crash-recovered exact temp on Windows.  Normal reads must keep
/// using [`open_existing_storage_file`].
pub(crate) fn open_existing_storage_file_read_write(
    parent: &Dir,
    name: impl AsRef<Path>,
    policy: PrivateStoragePolicy,
) -> Result<cap_std::fs::File> {
    match policy {
        PrivateStoragePolicy::Standard => {
            Ok(private_fs::open_private_file_read_write(parent, name, 1)?)
        }
        #[cfg(target_os = "android")]
        PrivateStoragePolicy::NativeAndroidNoBackup => Ok(
            private_fs::open_android_private_file_read_write(parent, name)?,
        ),
    }
}

pub(crate) fn open_existing_storage_dir(
    parent: &Dir,
    name: &str,
    policy: PrivateStoragePolicy,
) -> Result<Dir> {
    match policy {
        PrivateStoragePolicy::Standard => Ok(private_fs::open_private_dir(parent, name)?),
        #[cfg(target_os = "android")]
        PrivateStoragePolicy::NativeAndroidNoBackup => {
            if parent
                .symlink_metadata(name)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(Error::LocalExecutionJournalMismatch);
            }
            let mut options = OpenOptions::new();
            options
                .read(true)
                .follow(FollowSymlinks::No)
                .maybe_dir(true);
            let file = parent.open_with(name, &options)?;
            if !file.metadata()?.is_dir() {
                return Err(Error::LocalExecutionJournalMismatch);
            }
            let directory = Dir::from_std_file(file.into_std());
            private_fs::inspect_android_held_directory(&directory)?;
            Ok(directory)
        }
    }
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

fn migrate(connection: &mut Connection, expected_vault_id: Uuid) -> Result<()> {
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
    let after_v4: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if after_v4 == 5 {
        if !schema_v5_is_valid(&transaction)? {
            return Err(Error::InvalidSchema);
        }
        migrate_v5_to_v6(&transaction)?;
    }
    // v6 is local and unstaged.  Its additive proof families may be absent
    // only as complete families on an earlier local v6 database; partial
    // families are forensic corruption and are never auto-repaired.
    ensure_unshipped_v6_extension_schema(&transaction)?;
    if !schema_v6_is_valid(&transaction, expected_vault_id)? {
        return Err(Error::InvalidSchema);
    }
    transaction.commit()?;
    Ok(())
}

fn ensure_unshipped_v6_extension_schema(transaction: &Transaction<'_>) -> Result<()> {
    const FAMILIES: [&[&str]; 3] = [
        &[
            "local_execution_r3_bridge_receipts",
            "local_execution_bridge_receipt_operation_idx",
            "local_execution_bridge_receipts_no_update",
            "local_execution_bridge_receipts_no_delete",
        ],
        &[
            "local_execution_r3_consumption_anchors",
            "local_execution_consumption_anchor_operation_idx",
            "local_execution_consumption_anchors_no_update",
            "local_execution_consumption_anchors_no_delete",
        ],
        &[
            "mutation_retry_contracts",
            "mutation_retry_contract_operation_idx",
            "mutation_retry_contracts_no_update",
            "mutation_retry_contracts_no_delete",
        ],
    ];
    for names in FAMILIES {
        let present = names.iter().try_fold(0_i64, |count, name| {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
                [*name],
                |row| row.get(0),
            )?;
            Ok::<_, Error>(count + i64::from(exists))
        })?;
        if present == 0 {
            for (_, name, statement) in LOCAL_EXECUTION_SCHEMA_OBJECTS {
                if names.contains(&name) {
                    transaction.execute_batch(statement)?;
                }
            }
        } else if present != i64::try_from(names.len()).expect("constant length fits i64") {
            return Err(Error::InvalidSchema);
        }
    }
    Ok(())
}

fn create_schema(transaction: &Transaction<'_>) -> Result<()> {
    for (_, _, statement) in SCHEMA_OBJECTS_V5
        .iter()
        .chain(LOCAL_EXECUTION_SCHEMA_OBJECTS.iter())
    {
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
    transaction.pragma_update(None, "user_version", 5)?;
    Ok(())
}

fn migrate_v5_to_v6(transaction: &Transaction<'_>) -> Result<()> {
    // This migration is additive. Existing mutation rows do not contain local
    // verifier evidence, so they intentionally remain untouched and no
    // identity, execution, or recovery facts are inferred from them.
    for (_, _, statement) in LOCAL_EXECUTION_SCHEMA_OBJECTS {
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
    if !schema_definitions_are_exact(connection, &SCHEMA_OBJECTS_V5)? {
        return Ok(false);
    }
    let foreign_key_errors: i64 =
        connection.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    Ok(foreign_key_errors == 0)
}

fn schema_v6_is_valid(connection: &Connection, expected_vault_id: Uuid) -> Result<bool> {
    if !schema_definitions_are_exact_combined(
        connection,
        &SCHEMA_OBJECTS_V5,
        &LOCAL_EXECUTION_SCHEMA_OBJECTS,
    )? {
        return Ok(false);
    }
    let foreign_key_errors: i64 =
        connection.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_errors != 0
        || !local_execution_rows_are_semantically_valid(connection, expected_vault_id)?
    {
        return Ok(false);
    }
    let operations = connection
        .prepare("SELECT operation_id FROM mutation_intents")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for operation in operations {
        let Ok(operation) = parse_uuid(&operation) else {
            return Ok(false);
        };
        if !mutation_history_is_exact(connection, operation)? {
            return Ok(false);
        }
    }
    global_r3_5_relations_are_exact(connection)
}

fn global_r3_5_relations_are_exact(connection: &Connection) -> Result<bool> {
    let receipts: i64 = connection.query_row(
        "SELECT COUNT(*) FROM local_execution_r3_bridge_receipts",
        [],
        |row| row.get(0),
    )?;
    let anchors: i64 = connection.query_row(
        "SELECT COUNT(*) FROM local_execution_r3_consumption_anchors",
        [],
        |row| row.get(0),
    )?;
    if receipts != anchors {
        return Ok(false);
    }
    let unmatched_anchor: i64 = connection.query_row(
        "SELECT COUNT(*) FROM local_execution_r3_consumption_anchors AS anchor
          WHERE NOT EXISTS (
            SELECT 1 FROM local_execution_r3_bridge_receipts AS receipt
             WHERE receipt.receipt_id = anchor.receipt_id
               AND receipt.receipt_fingerprint = anchor.receipt_fingerprint
               AND receipt.operation_id = anchor.operation_id
               AND receipt.attempt_number = anchor.attempt_number
               AND receipt.outcome_id = anchor.outcome_id
               AND receipt.evidence_id = anchor.evidence_id
               AND receipt.r3_evidence_fingerprint = anchor.r3_evidence_fingerprint
               AND receipt.dependency_kind = anchor.dependency_kind)",
        [],
        |row| row.get(0),
    )?;
    if unmatched_anchor != 0 {
        return Ok(false);
    }
    // The count/join check above prevents a missing counterpart, but it does
    // not establish that either immutable row is the *canonical* encoding of
    // that counterpart.  Rebuild both preimages for every receipt.  This is
    // deliberately a reverse scan rather than an authority lookup: an
    // attacker must not be able to substitute a coherent-looking pair with
    // fresh UUIDs or fingerprints and retain reopen validity.
    if !global_bridge_receipts_and_anchors_are_canonical(connection)? {
        return Ok(false);
    }
    let retry_events: i64 = connection.query_row(
        "SELECT COUNT(*) FROM mutation_events WHERE phase = 'retry_scheduled'",
        [],
        |row| row.get(0),
    )?;
    let contracts: i64 =
        connection.query_row("SELECT COUNT(*) FROM mutation_retry_contracts", [], |row| {
            row.get(0)
        })?;
    if retry_events != contracts {
        return Ok(false);
    }
    let unmatched_contract: i64 = connection.query_row(
        "SELECT COUNT(*) FROM mutation_retry_contracts AS contract
          WHERE NOT EXISTS (
            SELECT 1 FROM mutation_events AS event
            JOIN mutation_verification_evidence AS evidence ON evidence.evidence_id = event.evidence_id
            JOIN mutation_intents AS intent ON intent.operation_id = event.operation_id
             WHERE event.operation_id = contract.operation_id
               AND event.state_version = contract.state_version
               AND event.phase = 'retry_scheduled'
               AND event.attempt_number = contract.attempt_number
               AND event.evidence_id = contract.evidence_id
               AND event.disposition = contract.disposition
               AND event.outcome_code IS contract.outcome_code
               AND event.occurred_at_unix_ms = contract.captured_at_unix_ms
               AND evidence.operation_id = contract.operation_id
               AND evidence.attempt_number = contract.attempt_number
               AND evidence.evidence_fingerprint = contract.evidence_fingerprint
               AND evidence.disposition = contract.disposition
               AND evidence.outcome_code IS contract.outcome_code
               AND evidence.captured_at_unix_ms = contract.captured_at_unix_ms
               AND contract.due_at_unix_ms >= contract.captured_at_unix_ms)", [], |row| row.get(0),
    )?;
    Ok(unmatched_contract == 0)
}

/// Recompute the complete receipt and consumption-anchor preimages for every
/// durable bridge row.  Foreign keys and the pair-count check are insufficient
/// here because the anchor table intentionally has no FK to the receipt: it
/// survives receipt/batch cleanup as an independent audit fact.
#[allow(clippy::too_many_lines)] // Complete canonical preimage must be read together.
fn global_bridge_receipts_and_anchors_are_canonical(connection: &Connection) -> Result<bool> {
    let mut statement = connection.prepare(
        "SELECT receipt_id, receipt_fingerprint, operation_id, attempt_number, boundary_id,
                boundary_occurred_at_unix_ms, contract_fingerprint, outcome_id, evidence_id,
                local_evidence_fingerprint, outcome_occurred_at_unix_ms, r3_intent_fingerprint,
                r3_evidence_fingerprint, r3_outcome_code, dependency_kind, r3_state_phase,
                r3_state_disposition, r3_attempt_number, r3_state_version, r3_last_evidence_id,
                r3_event_state_version
           FROM local_execution_r3_bridge_receipts",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Vec<u8>>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, Vec<u8>>(9)?,
            row.get::<_, i64>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, String>(12)?,
            row.get::<_, Option<String>>(13)?,
            row.get::<_, String>(14)?,
            row.get::<_, String>(15)?,
            row.get::<_, String>(16)?,
            row.get::<_, i64>(17)?,
            row.get::<_, i64>(18)?,
            row.get::<_, String>(19)?,
            row.get::<_, i64>(20)?,
        ))
    })?;
    for row in rows {
        let (
            receipt_id,
            receipt_fingerprint,
            operation_id,
            attempt_number,
            boundary_id,
            boundary_at,
            contract_fingerprint,
            outcome_id,
            evidence_id,
            local_evidence_fingerprint,
            outcome_at,
            r3_intent,
            r3_evidence,
            r3_outcome_code,
            dependency_kind,
            state_phase,
            state_disposition,
            r3_attempt,
            state_version,
            last_evidence_id,
            event_version,
        ) = row?;
        let (
            Ok(operation_id),
            Ok(boundary_id),
            Ok(outcome_id),
            Ok(evidence_id),
            Ok(last_evidence_id),
            Ok(receipt_fingerprint),
            Ok(contract_fingerprint),
            Ok(local_evidence_fingerprint),
            Ok(attempt_number),
            Ok(boundary_at),
            Ok(outcome_at),
            Ok(r3_attempt),
            Ok(state_version),
            Ok(event_version),
        ) = (
            parse_uuid(&operation_id),
            parse_uuid(&boundary_id),
            parse_uuid(&outcome_id),
            parse_uuid(&evidence_id),
            parse_uuid(&last_evidence_id),
            blob32(receipt_fingerprint),
            blob32(contract_fingerprint),
            blob32(local_evidence_fingerprint),
            u32::try_from(attempt_number),
            u64::try_from(boundary_at),
            u64::try_from(outcome_at),
            u32::try_from(r3_attempt),
            u64::try_from(state_version),
            u64::try_from(event_version),
        )
        else {
            return Ok(false);
        };
        if parse_canonical_sha256(&r3_intent).is_none()
            || parse_canonical_sha256(&r3_evidence).is_none()
            || r3_outcome_code
                .as_deref()
                .is_some_and(|value| validate_redacted_code(value).is_err())
            || bridge_dependency_kind_code(&dependency_kind).is_err()
        {
            return Ok(false);
        }
        let receipt = BridgeReceiptFacts {
            operation_id,
            attempt_number,
            boundary_id,
            boundary_occurred_at_unix_ms: boundary_at,
            contract_fingerprint,
            outcome_id,
            evidence_id,
            local_evidence_fingerprint,
            outcome_occurred_at_unix_ms: outcome_at,
            r3_intent_fingerprint: r3_intent,
            r3_evidence_fingerprint: r3_evidence,
            r3_outcome_code,
            dependency_kind,
            r3_state_phase: state_phase,
            r3_state_disposition: state_disposition,
            r3_attempt_number: r3_attempt,
            r3_state_version: state_version,
            r3_last_evidence_id: last_evidence_id,
            r3_event_state_version: event_version,
        };
        let expected_receipt_fingerprint = bridge_receipt_fingerprint(&receipt);
        if parse_uuid(&receipt_id).ok() != Some(bridge_receipt_id(expected_receipt_fingerprint))
            || receipt_fingerprint != expected_receipt_fingerprint
            || !consumption_anchor_is_exact(connection, &receipt)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Validates v6's encoded ledger invariants on every reopen.  `SQLite` CHECKs
/// and FKs protect new writes, but an interrupted/older or manually-corrupted
/// database must remain readable for forensics while refusing normal open.
/// Attestation bytes are intentionally not compared: they are first-issuance
/// audit material, while authority/equality uses the stable identity binding.
#[allow(clippy::too_many_lines)]
fn local_execution_rows_are_semantically_valid(
    connection: &Connection,
    expected_vault_id: Uuid,
) -> Result<bool> {
    local_execution_rows_are_semantically_valid_in_scope(connection, expected_vault_id, None)
}

/// Exact re-registration must validate the requested immutable contract, not
/// make a healthy operation hostage to a separately corrupt operation.  Full
/// reopen intentionally calls the same validator without a scope and remains
/// global/fail-closed.
fn local_execution_operation_rows_are_semantically_valid(
    connection: &Connection,
    expected_vault_id: Uuid,
    operation_id: Uuid,
) -> Result<bool> {
    local_execution_rows_are_semantically_valid_in_scope(
        connection,
        expected_vault_id,
        Some(operation_id),
    )
}

#[allow(clippy::too_many_lines)]
fn local_execution_rows_are_semantically_valid_in_scope(
    connection: &Connection,
    expected_vault_id: Uuid,
    operation_scope: Option<Uuid>,
) -> Result<bool> {
    let scoped = operation_scope.map(|value| value.to_string());
    let mut contracts = connection
        .prepare(if scoped.is_some() {
            "SELECT operation_id, vault_id, completion_id FROM local_execution_contracts WHERE operation_id = ?1"
        } else {
            "SELECT operation_id, vault_id, completion_id FROM local_execution_contracts WHERE (?1 IS NULL OR operation_id = ?1)"
        })?;
    for row in contracts.query_map((scoped.as_deref(),), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (operation, vault, completion) = row?;
        let Ok(operation) = parse_uuid(&operation) else {
            return Ok(false);
        };
        if operation.is_nil()
            || parse_uuid(&vault).map_or(true, |value| value != expected_vault_id)
            || completion != local_execution_completion_id(operation)
        {
            return Ok(false);
        }
    }
    {
        let mut statement = connection.prepare(if scoped.is_some() {
            "SELECT evidence_id, operation_id, role FROM local_execution_identity_evidence WHERE operation_id = ?1"
        } else { "SELECT evidence_id, operation_id, role FROM local_execution_identity_evidence WHERE (?1 IS NULL OR operation_id = ?1)" })?;
        for row in statement.query_map((scoped.as_deref(),), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })? {
            let (evidence_id, operation, role) = row?;
            let Ok(operation) = parse_uuid(&operation) else {
                return Ok(false);
            };
            if operation.is_nil()
                || !matches!(
                    role.as_str(),
                    "vault_root"
                        | "source_parent"
                        | "source_object"
                        | "destination_parent"
                        | "collision_parent_start"
                        | "collision_parent_end"
                )
                || evidence_id != format!("local-execution-identity:{operation}:{role}")
            {
                return Ok(false);
            }
        }
    }
    {
        let mut statement = connection.prepare(if scoped.is_some() {
            "SELECT boundary_id, operation_id, attempt_number, occurred_at_unix_ms
               FROM local_execution_attempt_boundaries WHERE operation_id = ?1"
        } else {
            "SELECT boundary_id, operation_id, attempt_number, occurred_at_unix_ms
               FROM local_execution_attempt_boundaries WHERE (?1 IS NULL OR operation_id = ?1)"
        })?;
        for row in statement.query_map((scoped.as_deref(),), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })? {
            let (identifier, operation, attempt, occurred_at) = row?;
            if parse_uuid(&identifier).map_or(true, |value| value.is_nil())
                || parse_uuid(&operation).map_or(true, |value| value.is_nil())
                || u32::try_from(attempt).is_err()
                || u64::try_from(occurred_at).is_err()
            {
                return Ok(false);
            }
        }
    }
    {
        let mut statement = connection.prepare(if scoped.is_some() {
            "SELECT outcome_id, evidence_id, operation_id, attempt_number, occurred_at_unix_ms
               FROM local_execution_attempt_outcomes WHERE operation_id = ?1"
        } else {
            "SELECT outcome_id, evidence_id, operation_id, attempt_number, occurred_at_unix_ms
               FROM local_execution_attempt_outcomes WHERE (?1 IS NULL OR operation_id = ?1)"
        })?;
        for row in statement.query_map((scoped.as_deref(),), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })? {
            let (outcome_id, evidence_id, operation, attempt, occurred_at) = row?;
            if parse_uuid(&outcome_id).map_or(true, |value| value.is_nil())
                || parse_uuid(&evidence_id).map_or(true, |value| value.is_nil())
                || parse_uuid(&operation).map_or(true, |value| value.is_nil())
                || u32::try_from(attempt).is_err()
                || u64::try_from(occurred_at).is_err()
            {
                return Ok(false);
            }
        }
    }
    // Complete typed roles, exact destination capture equality, deterministic
    // completion timestamp, contiguous ordinal/count relation, and every
    // boundary/outcome association must all still hold after reopen.
    let invalid: i64 = connection.query_row(
        "SELECT COUNT(*) FROM local_execution_contracts AS contract
          WHERE NOT EXISTS (
                SELECT 1 FROM local_execution_contract_completions AS completion
                 WHERE completion.completion_id = contract.completion_id
                   AND completion.operation_id = contract.operation_id
                   AND completion.completed_at_unix_ms = contract.registered_at_unix_ms)
             OR (SELECT COUNT(*) FROM local_execution_identity_evidence WHERE operation_id = contract.operation_id) != 6
             OR (SELECT COUNT(*) FROM local_execution_identity_evidence
                  WHERE operation_id = contract.operation_id
                    AND role IN ('vault_root','source_parent','source_object','destination_parent','collision_parent_start','collision_parent_end')) != 6
             OR (SELECT object_kind FROM local_execution_identity_evidence WHERE operation_id = contract.operation_id AND role = 'source_object') NOT IN (1,2)
             OR EXISTS (SELECT 1 FROM local_execution_identity_evidence WHERE operation_id = contract.operation_id AND role != 'source_object' AND object_kind != 1)
             OR (SELECT stable_identity_fingerprint FROM local_execution_identity_evidence WHERE operation_id = contract.operation_id AND role = 'destination_parent') !=
                (SELECT stable_identity_fingerprint FROM local_execution_identity_evidence WHERE operation_id = contract.operation_id AND role = 'collision_parent_start')
             OR (SELECT stable_identity_fingerprint FROM local_execution_identity_evidence WHERE operation_id = contract.operation_id AND role = 'destination_parent') !=
                (SELECT stable_identity_fingerprint FROM local_execution_identity_evidence WHERE operation_id = contract.operation_id AND role = 'collision_parent_end')
             OR (SELECT COUNT(*) FROM local_execution_collision_members WHERE operation_id = contract.operation_id) != contract.collision_member_count
             OR EXISTS (SELECT 1 FROM local_execution_collision_members WHERE operation_id = contract.operation_id AND ordinal NOT BETWEEN 0 AND contract.collision_member_count - 1)
             OR EXISTS (SELECT 1 FROM local_execution_attempt_boundaries AS boundary
                          WHERE boundary.operation_id = contract.operation_id
                            AND boundary.contract_fingerprint != contract.contract_fingerprint)
             OR EXISTS (SELECT 1 FROM local_execution_attempt_outcomes AS outcome
                          LEFT JOIN local_execution_attempt_boundaries AS boundary
                            ON boundary.operation_id = outcome.operation_id AND boundary.attempt_number = outcome.attempt_number
                         WHERE outcome.operation_id = contract.operation_id
                           AND (boundary.operation_id IS NULL OR outcome.contract_fingerprint != contract.contract_fingerprint
                                OR (outcome.outcome IN ('VerifiedApplied','VerifiedNotApplied')) != (outcome.non_retryable = 0)))
           AND (?1 IS NULL OR contract.operation_id = ?1)",
        [scoped.as_deref()],
        |row| row.get(0),
    )?;
    if invalid != 0 {
        return Ok(false);
    }
    local_execution_persisted_fingerprints_are_exact(connection, expected_vault_id, operation_scope)
}

#[allow(clippy::too_many_lines)]
fn local_execution_persisted_fingerprints_are_exact(
    connection: &Connection,
    expected_vault_id: Uuid,
    operation_scope: Option<Uuid>,
) -> Result<bool> {
    let scoped = operation_scope.map(|value| value.to_string());
    let mut contracts = connection.prepare(if scoped.is_some() {
        "SELECT operation_id, vault_id, intent_fingerprint, contract_fingerprint,
                target_name, target_collision_key, collision_member_count,
                collision_snapshot_fingerprint
           FROM local_execution_contracts WHERE operation_id = ?1"
    } else {
        "SELECT operation_id, vault_id, intent_fingerprint, contract_fingerprint,
                target_name, target_collision_key, collision_member_count,
                collision_snapshot_fingerprint
           FROM local_execution_contracts WHERE (?1 IS NULL OR operation_id = ?1)"
    })?;
    let rows = contracts.query_map((scoped.as_deref(),), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, Vec<u8>>(7)?,
        ))
    })?;
    for row in rows {
        let (operation, vault, intent, stored_contract, target, target_key, count, stored_snapshot) =
            row?;
        let (
            Ok(operation),
            Ok(vault),
            Ok(intent),
            Ok(stored_contract),
            Ok(stored_snapshot),
            Ok(count),
        ) = (
            parse_uuid(&operation),
            parse_uuid(&vault),
            blob32(intent),
            blob32(stored_contract),
            blob32(stored_snapshot),
            u32::try_from(count),
        )
        else {
            return Ok(false);
        };
        if operation.is_nil()
            || vault != expected_vault_id
            || persisted_canonical_collision_key(&target).ok().as_deref()
                != Some(target_key.as_str())
        {
            return Ok(false);
        }
        let mut identities = Vec::new();
        for role in [
            "vault_root",
            "source_parent",
            "source_object",
            "destination_parent",
            "collision_parent_start",
            "collision_parent_end",
        ] {
            let identity: Option<SemanticIdentityRow> = connection.query_row(
                "SELECT evidence_version, evidence_kind, object_kind, provider_id, object_id, stable_identity_fingerprint
                   FROM local_execution_identity_evidence WHERE operation_id = ?1 AND role = ?2",
                params![operation.to_string(), role],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            ).optional()?;
            let Some((version, kind, object_kind, provider_id, object_id, fingerprint)) = identity
            else {
                return Ok(false);
            };
            let (Ok(version), Ok(kind), Ok(object_kind), Ok(fingerprint)) = (
                u8::try_from(version),
                u8::try_from(kind),
                u8::try_from(object_kind),
                blob32(fingerprint),
            ) else {
                return Ok(false);
            };
            let identity = SemanticIdentity {
                version,
                kind,
                object_kind,
                provider_id,
                object_id,
                fingerprint,
            };
            if persisted_stable_identity_fingerprint(
                version,
                kind,
                object_kind,
                &identity.provider_id,
                &identity.object_id,
            )
            .ok()
                != Some(fingerprint)
                || (role != "source_object" && object_kind != 1)
            {
                return Ok(false);
            }
            identities.push(identity);
        }
        if identities[3].fingerprint != identities[4].fingerprint
            || identities[3].fingerprint != identities[5].fingerprint
        {
            return Ok(false);
        }
        let mut statement = connection.prepare(
            "SELECT ordinal, name, collision_key, evidence_version, evidence_kind, object_kind,
                    provider_id, object_id, stable_identity_fingerprint
               FROM local_execution_collision_members WHERE operation_id = ?1 ORDER BY ordinal",
        )?;
        let member_rows = statement.query_map([operation.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Vec<u8>>(6)?,
                row.get::<_, Vec<u8>>(7)?,
                row.get::<_, Vec<u8>>(8)?,
            ))
        })?;
        let mut members: Vec<SemanticCollisionMember> = Vec::new();
        for (ordinal, row) in member_rows.enumerate() {
            let (
                ordinal_db,
                name,
                key,
                version,
                kind,
                object_kind,
                provider_id,
                object_id,
                fingerprint,
            ) = row?;
            let (Ok(version), Ok(kind), Ok(object_kind), Ok(fingerprint)) = (
                u8::try_from(version),
                u8::try_from(kind),
                u8::try_from(object_kind),
                blob32(fingerprint),
            ) else {
                return Ok(false);
            };
            if ordinal_db != i64::try_from(ordinal).expect("ordinal fits i64")
                || persisted_canonical_collision_key(&name).ok().as_deref() != Some(key.as_str())
            {
                return Ok(false);
            }
            let identity = SemanticIdentity {
                version,
                kind,
                object_kind,
                provider_id,
                object_id,
                fingerprint,
            };
            if persisted_stable_identity_fingerprint(
                version,
                kind,
                object_kind,
                &identity.provider_id,
                &identity.object_id,
            )
            .ok()
                != Some(fingerprint)
            {
                return Ok(false);
            }
            let member = SemanticCollisionMember {
                name,
                collision_key: key,
                identity,
            };
            if let Some(previous) = members.last() {
                if (
                    previous.collision_key.as_str(),
                    previous.name.as_str(),
                    previous.identity.fingerprint,
                ) >= (
                    member.collision_key.as_str(),
                    member.name.as_str(),
                    member.identity.fingerprint,
                ) {
                    return Ok(false);
                }
            }
            members.push(member);
        }
        if members.len() != usize::try_from(count).map_err(|_| Error::InvalidSchema)?
            || semantic_collision_fingerprint(&identities[4], &identities[5], &members)
                != stored_snapshot
            || semantic_contract_fingerprint(
                operation,
                vault,
                intent,
                &identities[..4],
                &target,
                &target_key,
                &identities[4],
                &identities[5],
                &members,
            ) != stored_contract
        {
            return Ok(false);
        }
    }
    bridge_receipts_are_semantically_exact(connection, operation_scope)
}

#[allow(clippy::too_many_lines)]
fn bridge_receipts_are_semantically_exact(
    connection: &Connection,
    operation_scope: Option<Uuid>,
) -> Result<bool> {
    let scoped = operation_scope.map(|value| value.to_string());
    let mut statement = connection.prepare(
        if scoped.is_some() { "SELECT receipt_id, receipt_fingerprint, operation_id, attempt_number, boundary_id,
                boundary_occurred_at_unix_ms, contract_fingerprint, outcome_id, evidence_id,
                local_evidence_fingerprint, outcome_occurred_at_unix_ms, r3_intent_fingerprint,
                r3_evidence_fingerprint, r3_outcome_code, dependency_kind, r3_attempt_number, r3_state_version,
                r3_state_phase, r3_state_disposition, r3_last_evidence_id, r3_event_state_version
           FROM local_execution_r3_bridge_receipts WHERE operation_id = ?1" } else { "SELECT receipt_id, receipt_fingerprint, operation_id, attempt_number, boundary_id,
                boundary_occurred_at_unix_ms, contract_fingerprint, outcome_id, evidence_id,
                local_evidence_fingerprint, outcome_occurred_at_unix_ms, r3_intent_fingerprint,
                r3_evidence_fingerprint, r3_outcome_code, dependency_kind, r3_attempt_number, r3_state_version,
                r3_state_phase, r3_state_disposition, r3_last_evidence_id, r3_event_state_version
           FROM local_execution_r3_bridge_receipts WHERE (?1 IS NULL OR operation_id = ?1)" },
    )?;
    for row in statement.query_map((scoped.as_deref(),), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Vec<u8>>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, Vec<u8>>(9)?,
            row.get::<_, i64>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, String>(12)?,
            row.get::<_, Option<String>>(13)?,
            row.get::<_, String>(14)?,
            row.get::<_, i64>(15)?,
            row.get::<_, i64>(16)?,
            row.get::<_, String>(17)?,
            row.get::<_, String>(18)?,
            row.get::<_, String>(19)?,
            row.get::<_, i64>(20)?,
        ))
    })? {
        let (
            receipt_id,
            fingerprint,
            operation,
            attempt,
            boundary,
            boundary_at,
            contract,
            outcome,
            evidence,
            local_evidence,
            outcome_at,
            r3_intent,
            r3_evidence,
            r3_outcome_code,
            kind,
            r3_attempt,
            state_version,
            state_phase,
            state_disposition,
            last_evidence,
            event_version,
        ) = row?;
        let (
            Ok(fingerprint),
            Ok(operation),
            Ok(attempt),
            Ok(boundary),
            Ok(boundary_at),
            Ok(contract),
            Ok(outcome),
            Ok(evidence),
            Ok(local_evidence),
            Ok(outcome_at),
            Ok(r3_attempt),
            Ok(state_version),
            Ok(last_evidence),
            Ok(event_version),
        ) = (
            blob32(fingerprint),
            parse_uuid(&operation),
            u32::try_from(attempt),
            parse_uuid(&boundary),
            u64::try_from(boundary_at),
            blob32(contract),
            parse_uuid(&outcome),
            parse_uuid(&evidence),
            blob32(local_evidence),
            u64::try_from(outcome_at),
            u32::try_from(r3_attempt),
            u64::try_from(state_version),
            parse_uuid(&last_evidence),
            u64::try_from(event_version),
        )
        else {
            return Ok(false);
        };
        if receipt_id.is_empty()
            || operation.is_nil()
            || boundary.is_nil()
            || outcome.is_nil()
            || evidence.is_nil()
        {
            return Ok(false);
        }
        let receipt = BridgeReceiptFacts {
            operation_id: operation,
            attempt_number: attempt,
            boundary_id: boundary,
            boundary_occurred_at_unix_ms: boundary_at,
            contract_fingerprint: contract,
            outcome_id: outcome,
            evidence_id: evidence,
            local_evidence_fingerprint: local_evidence,
            outcome_occurred_at_unix_ms: outcome_at,
            r3_intent_fingerprint: r3_intent,
            r3_evidence_fingerprint: r3_evidence,
            r3_outcome_code,
            dependency_kind: kind,
            r3_state_phase: state_phase,
            r3_state_disposition: state_disposition,
            r3_attempt_number: r3_attempt,
            r3_state_version: state_version,
            r3_last_evidence_id: last_evidence,
            r3_event_state_version: event_version,
        };
        if parse_uuid(&receipt_id).ok() != Some(bridge_receipt_id(fingerprint))
            || bridge_receipt_fingerprint(&receipt) != fingerprint
            || !consumption_anchor_is_exact(connection, &receipt)?
            || !persisted_verified_applied_mutation_evidence_is_exact(connection, evidence)?
            || !mutation_history_is_exact(connection, operation)?
        {
            return Ok(false);
        }
        // R3 state versions are an append-only event sequence.  A receipt
        // cannot be rebound to a forged later final state by rewriting the
        // current state and its final event: every version through the bound
        // state must still be represented exactly once.
        let event_versions = connection
            .prepare(
                "SELECT state_version FROM mutation_events
                   WHERE operation_id = ?1 ORDER BY state_version",
            )?
            .query_map([operation.to_string()], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if usize::try_from(state_version)
            .ok()
            .and_then(|value| value.checked_add(1))
            != Some(event_versions.len())
            || event_versions.iter().enumerate().any(|(expected, actual)| {
                *actual != i64::try_from(expected).expect("state version fits i64")
            })
        {
            return Ok(false);
        }
        let joined: bool = connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM local_execution_contracts AS contract
               JOIN local_execution_attempt_boundaries AS boundary ON boundary.operation_id = contract.operation_id
               JOIN local_execution_attempt_outcomes AS outcome ON outcome.operation_id = boundary.operation_id AND outcome.attempt_number = boundary.attempt_number
               JOIN local_execution_r3_bridge_receipts AS receipt ON receipt.operation_id = outcome.operation_id AND receipt.attempt_number = outcome.attempt_number
               JOIN mutation_intents AS intent ON intent.operation_id = contract.operation_id
               JOIN mutation_verification_evidence AS evidence ON evidence.operation_id = intent.operation_id
               JOIN mutation_state AS state ON state.operation_id = intent.operation_id
               JOIN mutation_events AS event ON event.operation_id = state.operation_id AND event.attempt_number = state.attempt_number AND event.state_version = state.state_version AND event.evidence_id = state.last_evidence_id
              WHERE contract.operation_id = ?1 AND boundary.attempt_number = ?2 AND boundary.boundary_id = ?3
                AND boundary.occurred_at_unix_ms = ?4 AND contract.contract_fingerprint = ?5
                AND outcome.outcome_id = ?6 AND outcome.evidence_id = ?7 AND outcome.evidence_fingerprint = ?8 AND outcome.occurred_at_unix_ms = ?9 AND outcome.outcome = 'VerifiedApplied'
                AND intent.intent_fingerprint = ?10 AND contract.intent_fingerprint = ?11
                AND evidence.evidence_id = ?7 AND evidence.attempt_number = ?12 AND evidence.evidence_fingerprint = ?13
                AND evidence.capture_phase = 'post_verify' AND evidence.disposition = 'verified_applied' AND evidence.forbidden_side_effect = 0
                AND receipt.r3_outcome_code IS evidence.outcome_code
                AND evidence.outcome_code IS state.outcome_code
                AND state.outcome_code IS event.outcome_code
                AND state.phase = ?14 AND state.disposition = ?15 AND state.attempt_number = ?12 AND state.state_version = ?16 AND state.last_evidence_id = ?17
                AND event.phase = ?14 AND event.disposition = ?15 AND event.evidence_id = ?17 AND event.state_version = ?18)",
            params![operation.to_string(), i64::from(attempt), boundary.to_string(), u64_to_i64(boundary_at)?, contract.as_slice(),
                outcome.to_string(), evidence.to_string(), local_evidence.as_slice(), u64_to_i64(outcome_at)?,
                receipt.r3_intent_fingerprint, local_intent_fingerprint_from_r3_intent(&receipt.r3_intent_fingerprint)
                    .map_err(|_| Error::InvalidSchema)?.as_slice(), i64::from(r3_attempt), receipt.r3_evidence_fingerprint,
                receipt.r3_state_phase, receipt.r3_state_disposition, u64_to_i64(state_version)?,
                receipt.r3_last_evidence_id.to_string(), u64_to_i64(event_version)?],
            |row| row.get(0),
        )?;
        if !joined
            || authoritative_outcome_id(
                operation,
                attempt,
                boundary,
                boundary_at,
                evidence,
                local_evidence,
                LocalExecutionOutcome::VerifiedApplied,
                outcome_at,
            ) != outcome
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Clone)]
struct SemanticIdentity {
    version: u8,
    kind: u8,
    object_kind: u8,
    provider_id: Vec<u8>,
    object_id: Vec<u8>,
    fingerprint: [u8; 32],
}

#[derive(Clone)]
struct SemanticCollisionMember {
    name: String,
    collision_key: String,
    identity: SemanticIdentity,
}

fn semantic_append_bytes(material: &mut Vec<u8>, field: &[u8], value: &[u8]) {
    material.extend_from_slice(&(field.len() as u64).to_be_bytes());
    material.extend_from_slice(field);
    material.extend_from_slice(&(value.len() as u64).to_be_bytes());
    material.extend_from_slice(value);
}

fn semantic_append_identity(material: &mut Vec<u8>, identity: &SemanticIdentity) {
    semantic_append_bytes(material, b"evidence_version", &[identity.version]);
    semantic_append_bytes(material, b"evidence_kind", &[identity.kind]);
    semantic_append_bytes(material, b"object_kind", &[identity.object_kind]);
    semantic_append_bytes(material, b"provider_id", &identity.provider_id);
    semantic_append_bytes(material, b"object_id", &identity.object_id);
}

fn semantic_collision_fingerprint(
    start: &SemanticIdentity,
    end: &SemanticIdentity,
    members: &[SemanticCollisionMember],
) -> [u8; 32] {
    let mut material = Vec::new();
    semantic_append_bytes(
        &mut material,
        b"contract_version",
        b"myvault-r3.5-local-identity-v1",
    );
    semantic_append_bytes(&mut material, b"domain", b"collision-snapshot");
    semantic_append_identity(&mut material, start);
    semantic_append_identity(&mut material, end);
    semantic_append_bytes(
        &mut material,
        b"member_count",
        &u32::try_from(members.len())
            .expect("validated member bound")
            .to_be_bytes(),
    );
    for member in members {
        semantic_append_bytes(&mut material, b"member_name", member.name.as_bytes());
        semantic_append_bytes(
            &mut material,
            b"member_collision_key",
            member.collision_key.as_bytes(),
        );
        semantic_append_identity(&mut material, &member.identity);
    }
    Sha256::digest(material).into()
}

#[allow(clippy::too_many_arguments)]
fn semantic_contract_fingerprint(
    operation_id: Uuid,
    vault_id: Uuid,
    intent: [u8; 32],
    identities: &[SemanticIdentity],
    target_name: &str,
    target_collision_key: &str,
    start: &SemanticIdentity,
    end: &SemanticIdentity,
    members: &[SemanticCollisionMember],
) -> [u8; 32] {
    let mut material = Vec::new();
    semantic_append_bytes(
        &mut material,
        b"contract_version",
        b"myvault-r3.5-local-identity-v1",
    );
    semantic_append_bytes(&mut material, b"domain", b"durable-execution-binding");
    semantic_append_bytes(&mut material, b"operation_id", operation_id.as_bytes());
    semantic_append_bytes(&mut material, b"vault_id", vault_id.as_bytes());
    semantic_append_bytes(&mut material, b"intent_sha256", &intent);
    for identity in identities {
        semantic_append_identity(&mut material, identity);
    }
    semantic_append_bytes(&mut material, b"target_name", target_name.as_bytes());
    semantic_append_bytes(
        &mut material,
        b"target_collision_key",
        target_collision_key.as_bytes(),
    );
    semantic_append_identity(&mut material, start);
    semantic_append_identity(&mut material, end);
    semantic_append_bytes(
        &mut material,
        b"member_count",
        &u32::try_from(members.len())
            .expect("validated member bound")
            .to_be_bytes(),
    );
    for member in members {
        semantic_append_bytes(&mut material, b"member_name", member.name.as_bytes());
        semantic_append_bytes(
            &mut material,
            b"member_collision_key",
            member.collision_key.as_bytes(),
        );
        semantic_append_identity(&mut material, &member.identity);
    }
    Sha256::digest(material).into()
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

fn schema_definitions_are_exact_combined(
    connection: &Connection,
    first: &[(&str, &str, &str)],
    second: &[(&str, &str, &str)],
) -> Result<bool> {
    let expected = first
        .iter()
        .chain(second.iter())
        .copied()
        .collect::<Vec<_>>();
    schema_definitions_are_exact(connection, &expected)
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

#[cfg(test)]
mod local_execution_tests {
    use super::*;
    use crate::local_identity::{
        test_durable_execution_binding, test_durable_execution_binding_for_r3_intent,
        test_durable_execution_binding_with_attestation_offset, DurableExecutionBinding,
    };
    use crate::local_orchestration::{
        classify_authoritative_final_outcome, handle_local_execution_echo_hint,
        test_authoritative_evidence, test_authoritative_evidence_with_identity,
        EchoHintDisposition, LocalExecutionEchoHint, LocalExecutionEchoSource, PlatformCallFact,
    };
    use rusqlite::Connection;
    use std::fs;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use tempfile::TempDir;

    struct Fixture {
        _temporary: TempDir,
        app_data: PathBuf,
        vault: PathBuf,
        vault_id: Uuid,
    }

    impl Fixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().expect("temporary root");
            let root = temporary.path().canonicalize().expect("canonical root");
            let app_data = root.join("private-app-data");
            let vault = root.join("Vault");
            fs::create_dir(&app_data).expect("app data");
            fs::create_dir(&vault).expect("vault");
            make_private(&app_data);
            Self {
                _temporary: temporary,
                app_data,
                vault,
                vault_id: Uuid::new_v4(),
            }
        }

        fn open(&self) -> SyncStore {
            SyncStore::open(&self.app_data, &self.vault, self.vault_id).expect("sync store")
        }

        fn journal_directory(&self) -> PathBuf {
            self.app_data
                .join(ROOT_DIRECTORY)
                .join(VERSION_DIRECTORY)
                .join(VAULTS_DIRECTORY)
                .join(self.vault_id.to_string())
                .join(crate::sync_journal::JOURNAL_DIRECTORY)
        }
    }

    #[cfg(unix)]
    fn make_private(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private mode");
    }

    #[cfg(not(unix))]
    fn make_private(_path: &Path) {}

    #[cfg(unix)]
    fn make_private_file(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private file mode");
    }

    #[cfg(not(unix))]
    fn make_private_file(_path: &Path) {}

    fn sync_journal_temporary_count(directory: &Path) -> usize {
        fs::read_dir(directory)
            .expect("read journal directory")
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".sync-execution-witness-")
            })
            .count()
    }

    fn count(database_path: &Path, table: &str) -> i64 {
        Connection::open(database_path)
            .expect("read connection")
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("row count")
    }

    fn install_abort_trigger(database_path: &Path, name: &str, table: &str) {
        Connection::open(database_path)
            .expect("fault connection")
            .execute_batch(&format!(
                "CREATE TRIGGER {name} BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT, 'injected fault'); END;"
            ))
            .expect("install fault trigger");
    }

    fn prepared_journal_attempt() -> (
        Fixture,
        SyncStore,
        DurableExecutionBinding,
        LocalExecutionAttemptBoundary,
    ) {
        let fixture = Fixture::new();
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        let mut store = fixture.open();
        store
            .register_local_execution_contract(&binding, 10)
            .expect("contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("boundary");
        (fixture, store, binding, boundary)
    }

    fn ready_store(fixture: &Fixture) -> SyncStore {
        let mut store = fixture.open();
        let binding =
            VerifiedRemoteBinding::new("account-a", "remote-root", "account-a", "remote-root")
                .expect("binding");
        store.bind_remote_root(&binding, 1).expect("bind");
        store.begin_initial_scan("start-token", 2).expect("scan");
        store
            .apply_scan_page(
                None,
                &ScanPage {
                    entries: Vec::new(),
                    next_page_token: None,
                },
                3,
            )
            .expect("scan page");
        store
            .apply_changes_page(
                "start-token",
                &ChangesPage {
                    changes: Vec::new(),
                    next_page_token: None,
                    new_start_page_token: Some("cursor-1".into()),
                },
                4,
            )
            .expect("changes page");
        store
    }

    fn test_hash(byte: u8) -> String {
        std::iter::repeat_n(char::from(byte), 64).collect()
    }

    fn local_publish_intent(operation_id: Uuid) -> MutationIntent {
        let mut intent = MutationIntent {
            operation_id,
            operation_kind: MutationOperationKind::LocalPublish,
            account_id: None,
            remote_root_id: None,
            remote_file_id: None,
            source_parent_id: None,
            destination_parent_id: None,
            local_object_id: None,
            source_path: Some("notes/local-execution.md".into()),
            destination_path: None,
            expected_local_revision: Some("revision-a".into()),
            expected_remote_revision: None,
            base_reference: None,
            base_local_revision: None,
            base_remote_revision: None,
            base_sha256: None,
            base_byte_length: None,
            expected_local_sha256: Some(test_hash(b'a')),
            expected_local_byte_length: Some(1),
            expected_remote_sha256: None,
            expected_remote_byte_length: None,
            operation_marker: format!("r3.5-local-{operation_id}"),
            intent_fingerprint: String::new(),
            registered_at_unix_ms: 10,
        };
        intent.intent_fingerprint = intent.canonical_fingerprint();
        intent
    }

    fn local_publish_evidence(
        evidence_id: Uuid,
        intent: &MutationIntent,
    ) -> MutationVerificationEvidence {
        local_publish_evidence_with_outcome_code(
            evidence_id,
            intent,
            Some("verified_applied".into()),
        )
    }

    fn local_publish_evidence_with_outcome_code(
        evidence_id: Uuid,
        intent: &MutationIntent,
        outcome_code: Option<String>,
    ) -> MutationVerificationEvidence {
        let mut evidence = MutationVerificationEvidence {
            evidence_id,
            operation_id: intent.operation_id,
            attempt_number: 0,
            capture_phase: MutationEvidenceCapturePhase::PostVerify,
            disposition: MutationDisposition::VerifiedApplied,
            outcome_code,
            observed_account_id: None,
            observed_remote_root_id: None,
            observed_remote_file_id: None,
            observed_parent_id: None,
            observed_path: intent.source_path.clone(),
            observed_local_revision: intent.expected_local_revision.clone(),
            observed_remote_revision: None,
            observed_sha256: intent.expected_local_sha256.clone(),
            observed_byte_length: intent.expected_local_byte_length,
            observed_operation_marker: Some(intent.operation_marker.clone()),
            forbidden_side_effect: false,
            verified_received_byte_offset: None,
            resume_reference: None,
            evidence_fingerprint: String::new(),
            captured_at_unix_ms: 20,
        };
        evidence.evidence_fingerprint = evidence.canonical_fingerprint();
        evidence
    }

    fn scheduled_retry(
        store: &mut SyncStore,
        retry_safe: bool,
    ) -> (MutationIntent, MutationVerificationEvidence, MutationState) {
        let intent = local_publish_intent(Uuid::new_v4());
        store
            .register_mutation_intent(&intent, None)
            .expect("retry intent");
        store
            .claim_mutation(intent.operation_id, 0, 12)
            .expect("claim");
        let mut evidence = local_publish_evidence(Uuid::new_v4(), &intent);
        evidence.capture_phase = MutationEvidenceCapturePhase::Reconcile;
        let transition = if retry_safe {
            evidence.disposition = MutationDisposition::RetrySafe;
            evidence.outcome_code = Some("retry_safe".into());
            evidence.resume_reference = Some("resume-ref-a".into());
            evidence.verified_received_byte_offset = Some(0);
            MutationOutcomeTransition::RetrySafe {
                next_attempt_at_unix_ms: 25,
                resume_reference: "resume-ref-a".into(),
            }
        } else {
            evidence.disposition = MutationDisposition::VerifiedNotApplied;
            evidence.outcome_code = Some("verified_not_applied".into());
            evidence.forbidden_side_effect = true;
            MutationOutcomeTransition::VerifiedNotApplied {
                next_attempt_at_unix_ms: 25,
            }
        };
        evidence.evidence_fingerprint = evidence.canonical_fingerprint();
        let state = store
            .record_mutation_outcome(intent.operation_id, 1, &evidence, &transition)
            .expect("schedule retry");
        assert_eq!(state.phase, MutationPhase::RetryScheduled);
        (intent, evidence, state)
    }

    fn assert_tampered_retry_rejects_live_and_reopen(
        fixture: &Fixture,
        mut store: SyncStore,
        operation_id: Uuid,
        state_version: u64,
    ) {
        let state_before = store.mutation_state(operation_id).expect("state");
        let events_before: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM mutation_events WHERE operation_id = ?1",
                [operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("event count");
        let evidence_before: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM mutation_verification_evidence WHERE operation_id = ?1",
                [operation_id.to_string()],
                |row| row.get(0),
            )
            .expect("evidence count");
        assert!(matches!(
            store.claim_mutation(operation_id, state_version, 25),
            Err(Error::LocalMutationIncomplete)
        ));
        assert_eq!(
            store.mutation_state(operation_id).expect("unchanged state"),
            state_before
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM mutation_events WHERE operation_id = ?1",
                    [operation_id.to_string()],
                    |row| row.get::<_, i64>(0)
                )
                .expect("events"),
            events_before
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM mutation_verification_evidence WHERE operation_id = ?1",
                    [operation_id.to_string()],
                    |row| row.get::<_, i64>(0)
                )
                .expect("evidence"),
            evidence_before
        );
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Matrix keeps every fail-closed proof visible together.
    fn retry_contract_forensic_tamper_matrix_rejects_before_claim_and_on_reopen() {
        // Canonical evidence rewrites remain syntactically valid and update
        // the contract fingerprint too; the retry phase semantics must still
        // reject them before the claim writes a new event/state.
        for forbidden in [true, false] {
            let fixture = Fixture::new();
            let mut store = ready_store(&fixture);
            let (intent, mut evidence, state) = scheduled_retry(&mut store, false);
            evidence.capture_phase = MutationEvidenceCapturePhase::PostVerify;
            evidence.forbidden_side_effect = forbidden;
            evidence.evidence_fingerprint = evidence.canonical_fingerprint();
            store
                .connection
                .execute_batch(
                    "DROP TRIGGER mutation_evidence_no_update;
                 DROP TRIGGER mutation_retry_contracts_no_update;",
                )
                .expect("drop immutable guards");
            store
                .connection
                .execute(
                    "UPDATE mutation_verification_evidence
                    SET capture_phase = ?1, forbidden_side_effect = ?2, evidence_fingerprint = ?3
                  WHERE evidence_id = ?4",
                    params![
                        evidence.capture_phase.as_str(),
                        i64::from(evidence.forbidden_side_effect),
                        evidence.evidence_fingerprint,
                        evidence.evidence_id.to_string()
                    ],
                )
                .expect("rewrite canonical evidence");
            store
                .connection
                .execute(
                    "UPDATE mutation_retry_contracts SET evidence_fingerprint = ?1
                  WHERE operation_id = ?2 AND state_version = ?3",
                    params![
                        evidence.evidence_fingerprint,
                        intent.operation_id.to_string(),
                        u64_to_i64(state.state_version).expect("version")
                    ],
                )
                .expect("bind rewritten fingerprint");
            store.connection.execute_batch(&format!(
                "{MUTATION_EVIDENCE_NO_UPDATE_TRIGGER};{MUTATION_RETRY_CONTRACTS_NO_UPDATE_TRIGGER};"
            )).expect("restore guards");
            assert_tampered_retry_rejects_live_and_reopen(
                &fixture,
                store,
                intent.operation_id,
                state.state_version,
            );
        }

        // Missing current contract and an extra otherwise-CHECK-valid contract
        // must both fail history/cardinality before claim mutation.
        for extra in [false, true] {
            let fixture = Fixture::new();
            let mut store = ready_store(&fixture);
            let (intent, _evidence, state) = scheduled_retry(&mut store, false);
            if extra {
                store.connection.execute(
                    "INSERT INTO mutation_retry_contracts(operation_id, state_version, attempt_number,
                        evidence_id, evidence_fingerprint, disposition, outcome_code, due_at_unix_ms,
                        retry_mode, resume_reference, verified_received_byte_offset, captured_at_unix_ms)
                     VALUES (?1, 99, 99, ?2, ?3, 'verified_not_applied', 'verified_not_applied',
                        25, 'restart_exact', NULL, NULL, 20)",
                    params![intent.operation_id.to_string(), Uuid::new_v4().to_string(), "a".repeat(64)],
                ).expect("insert extra contract");
            } else {
                store
                    .connection
                    .execute_batch("DROP TRIGGER mutation_retry_contracts_no_delete;")
                    .expect("drop delete guard");
                assert_eq!(store.connection.execute("DELETE FROM mutation_retry_contracts WHERE operation_id = ?1 AND state_version = ?2", params![intent.operation_id.to_string(), u64_to_i64(state.state_version).expect("version")]).expect("delete contract"), 1);
                store
                    .connection
                    .execute_batch(MUTATION_RETRY_CONTRACTS_NO_DELETE_TRIGGER)
                    .expect("restore delete guard");
            }
            assert_tampered_retry_rejects_live_and_reopen(
                &fixture,
                store,
                intent.operation_id,
                state.state_version,
            );
        }

        // Bound fields are individually tampered while retaining SQL-valid
        // values; state/evidence/contract exact equality, not a CHECK, rejects.
        for (retry_safe, sql) in [
            (false, "UPDATE mutation_retry_contracts SET due_at_unix_ms = 26 WHERE operation_id = ?1 AND state_version = ?2"),
            (true, "UPDATE mutation_retry_contracts SET resume_reference = 'resume-ref-b' WHERE operation_id = ?1 AND state_version = ?2"),
            (true, "UPDATE mutation_retry_contracts SET verified_received_byte_offset = 1 WHERE operation_id = ?1 AND state_version = ?2"),
            (true, "UPDATE mutation_retry_contracts SET disposition = 'verified_not_applied', outcome_code = 'verified_not_applied', retry_mode = 'restart_exact', resume_reference = NULL, verified_received_byte_offset = NULL WHERE operation_id = ?1 AND state_version = ?2"),
        ] {
            let fixture = Fixture::new();
            let mut store = ready_store(&fixture);
            let (intent, _evidence, state) = scheduled_retry(&mut store, retry_safe);
            store
                .connection
                .execute_batch("DROP TRIGGER mutation_retry_contracts_no_update;")
                .expect("drop update guard");
            assert_eq!(store.connection.execute(sql, params![intent.operation_id.to_string(), u64_to_i64(state.state_version).expect("version")]).expect("tamper bound field"), 1);
            store
                .connection
                .execute_batch(MUTATION_RETRY_CONTRACTS_NO_UPDATE_TRIGGER)
                .expect("restore update guard");
            assert_tampered_retry_rejects_live_and_reopen(&fixture, store, intent.operation_id, state.state_version);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Keeps the coherent forensic rewrite matrix explicit.
    fn blocked_initial_history_canonical_rewrites_fail_closed_without_state_or_event_mutation() {
        for rewrite in [
            "capture_phase",
            "forbidden_side_effect",
            "observed_path",
            "observed_hash_and_length",
        ] {
            let fixture = Fixture::new();
            let operation_id = Uuid::new_v4();
            let input = RemoteExistingBlockedInput {
                account_id: "account-a".into(),
                remote_root_id: "remote-root".into(),
                remote_file_id: "remote-file".into(),
                source_parent_id: "remote-parent".into(),
                source_path: "notes/blocked.md".into(),
                local_object_id: None,
                expected_local_revision: "local-revision".into(),
                expected_local_sha256: test_hash(b'a'),
                expected_local_byte_length: 1,
                expected_remote_revision: "remote-revision".into(),
                expected_remote_sha256: Some(test_hash(b'b')),
                expected_remote_byte_length: Some(2),
                base_reference: None,
                base_local_revision: None,
                base_remote_revision: None,
                base_sha256: None,
                base_byte_length: None,
            };
            let (intent, mut evidence) =
                MutationIntent::remote_existing_blocked(operation_id, input, 10)
                    .expect("blocked initial evidence");
            let mut store = fixture.open();
            store
                .register_mutation_intent(&intent, Some(&evidence))
                .expect("register exact blocked intent");
            let original_evidence = evidence.clone();
            let state_before = store
                .mutation_state(operation_id)
                .expect("state")
                .expect("blocked state");
            let events_before = store.mutation_events(operation_id).expect("events");
            let evidence_count_before =
                count(store.database_path(), "mutation_verification_evidence");

            match rewrite {
                "capture_phase" => evidence.capture_phase = MutationEvidenceCapturePhase::Reconcile,
                "forbidden_side_effect" => evidence.forbidden_side_effect = false,
                "observed_path" => evidence.observed_path = Some("notes/rewritten.md".into()),
                "observed_hash_and_length" => {
                    evidence.observed_sha256 = Some(test_hash(b'c'));
                    evidence.observed_byte_length = Some(3);
                }
                _ => unreachable!("fixed rewrite matrix"),
            }
            evidence.evidence_fingerprint = evidence.canonical_fingerprint();
            store
                .connection
                .execute_batch("DROP TRIGGER mutation_evidence_no_update;")
                .expect("remove immutable evidence guard for forensic rewrite");
            store
                .connection
                .execute(
                    "UPDATE mutation_verification_evidence
                        SET capture_phase = ?1, forbidden_side_effect = ?2,
                            observed_path = ?3, observed_sha256 = ?4,
                            observed_byte_length = ?5, evidence_fingerprint = ?6
                      WHERE evidence_id = ?7",
                    params![
                        evidence.capture_phase.as_str(),
                        i64::from(evidence.forbidden_side_effect),
                        evidence.observed_path.clone(),
                        evidence.observed_sha256.clone(),
                        evidence
                            .observed_byte_length
                            .map(u64_to_i64)
                            .transpose()
                            .expect("test byte length"),
                        evidence.evidence_fingerprint.clone(),
                        evidence.evidence_id.to_string(),
                    ],
                )
                .expect("install self-canonical forensic rewrite");
            store
                .connection
                .execute_batch(MUTATION_EVIDENCE_NO_UPDATE_TRIGGER)
                .expect("restore immutable evidence guard");

            assert!(
                !mutation_history_is_exact(&store.connection, operation_id)
                    .expect("live history validation"),
                "{rewrite} must not become an accepted initial NeedsReconcile history"
            );
            assert_eq!(
                store
                    .mutation_state(operation_id)
                    .expect("state after rejection"),
                Some(state_before.clone()),
                "live validation must not mutate state"
            );
            assert_eq!(
                store
                    .mutation_events(operation_id)
                    .expect("events after rejection"),
                events_before,
                "live validation must not append or rewrite events"
            );
            assert_eq!(
                count(store.database_path(), "mutation_verification_evidence"),
                evidence_count_before,
                "live validation must not create evidence"
            );
            assert!(matches!(
                store.register_mutation_intent(&intent, Some(&original_evidence)),
                Err(Error::LocalMutationIncomplete)
            ));
            assert_eq!(
                store
                    .mutation_state(operation_id)
                    .expect("state after public rejection"),
                Some(state_before),
                "idempotent rejection must not mutate state"
            );
            assert_eq!(
                store
                    .mutation_events(operation_id)
                    .expect("events after public rejection"),
                events_before,
                "idempotent rejection must not append or rewrite events"
            );
            assert_eq!(
                count(store.database_path(), "mutation_verification_evidence"),
                evidence_count_before,
                "idempotent rejection must not create evidence"
            );
            drop(store);
            assert!(matches!(
                SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
                Err(Error::InvalidSchema)
            ));
        }
    }

    #[allow(clippy::type_complexity)]
    fn bridged_r3_5_batch(
        fixture: &Fixture,
    ) -> (
        SyncStore,
        DurableExecutionBinding,
        LocalExecutionAttemptBoundary,
        MutationIntent,
        MutationVerificationEvidence,
        AuthoritativeFinalOutcome,
        ChangeBatchDependency,
        Uuid,
    ) {
        bridged_r3_5_batch_with_outcome_code(fixture, Some("verified_applied".into()))
    }

    #[allow(clippy::type_complexity)]
    fn bridged_r3_5_batch_with_outcome_code(
        fixture: &Fixture,
        outcome_code: Option<String>,
    ) -> (
        SyncStore,
        DurableExecutionBinding,
        LocalExecutionAttemptBoundary,
        MutationIntent,
        MutationVerificationEvidence,
        AuthoritativeFinalOutcome,
        ChangeBatchDependency,
        Uuid,
    ) {
        let mut store = ready_store(fixture);
        let operation_id = Uuid::new_v4();
        let intent = local_publish_intent(operation_id);
        let binding = test_durable_execution_binding_for_r3_intent(
            operation_id,
            fixture.vault_id,
            &intent.intent_fingerprint,
        );
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        store
            .register_local_execution_contract(&binding, 10)
            .expect("contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("boundary");
        store
            .publish_local_execution_pre_side_effect_witness(&binding, &boundary, 11)
            .expect("pre witness");
        store
            .register_mutation_intent(&intent, None)
            .expect("intent");
        store.claim_mutation(operation_id, 0, 12).expect("claim");
        let evidence =
            local_publish_evidence_with_outcome_code(Uuid::new_v4(), &intent, outcome_code);
        let r3_fingerprint = parse_canonical_sha256(&evidence.evidence_fingerprint)
            .expect("canonical R3 fingerprint");
        store
            .record_mutation_outcome(
                operation_id,
                1,
                &evidence,
                &MutationOutcomeTransition::VerifiedApplied,
            )
            .expect("R3 completion");
        let dependency = ChangeBatchDependency {
            operation_id,
            kind: ChangeBatchDependencyKind::Mutation,
        };
        let batch_id = Uuid::new_v4();
        store
            .begin_r3_change_batch(batch_id, "cursor-1", "cursor-2", &[dependency])
            .expect("batch");
        let decision = classify_authoritative_final_outcome(
            &binding,
            &boundary,
            test_authoritative_evidence_with_identity(
                &binding,
                &boundary,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
                evidence.evidence_id,
                r3_fingerprint,
            ),
        )
        .expect("authoritative decision");
        store
            .finalize_authoritative_local_execution_outcome(&binding, &boundary, &decision)
            .expect("local finalization");
        store
            .commit_r3_5_verified_local_execution_dependency(
                batch_id, dependency, &binding, 0, &decision,
            )
            .expect("bridge receipt");
        (
            store, binding, boundary, intent, evidence, decision, dependency, batch_id,
        )
    }

    fn bridge_receipt_for(
        binding: &DurableExecutionBinding,
        boundary: &LocalExecutionAttemptBoundary,
        intent: &MutationIntent,
        evidence: &MutationVerificationEvidence,
        decision: &AuthoritativeFinalOutcome,
        dependency: ChangeBatchDependency,
    ) -> BridgeReceiptFacts {
        BridgeReceiptFacts {
            operation_id: boundary.operation_id,
            attempt_number: boundary.attempt_number,
            boundary_id: boundary.boundary_id,
            boundary_occurred_at_unix_ms: boundary.occurred_at_unix_ms,
            contract_fingerprint: *binding.fingerprint().as_bytes(),
            outcome_id: decision.outcome_id(),
            evidence_id: evidence.evidence_id,
            local_evidence_fingerprint: decision.evidence_fingerprint(),
            outcome_occurred_at_unix_ms: decision.recorded_at_unix_ms(),
            r3_intent_fingerprint: intent.intent_fingerprint.clone(),
            r3_evidence_fingerprint: evidence.evidence_fingerprint.clone(),
            r3_outcome_code: evidence.outcome_code.clone(),
            dependency_kind: dependency.kind.as_str().to_owned(),
            r3_state_phase: "completed".to_owned(),
            r3_state_disposition: "verified_applied".to_owned(),
            r3_attempt_number: 0,
            r3_state_version: 2,
            r3_last_evidence_id: evidence.evidence_id,
            r3_event_state_version: 2,
        }
    }

    #[test]
    fn equal_cardinality_substituted_anchor_and_partial_extension_family_fail_reopen() {
        let fixture = Fixture::new();
        let (store, _binding, _boundary, _intent, _evidence, _decision, _dependency, _batch) =
            bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        // Change only the canonical anchor ID.  Counts and copied receipt
        // fields remain equal, so this reaches canonical reverse validation
        // rather than an FK/CHECK or cardinality rejection.
        store
            .connection
            .execute_batch("DROP TRIGGER local_execution_consumption_anchors_no_update;")
            .expect("drop anchor update guard");
        assert_eq!(
            store
                .connection
                .execute(
                    "UPDATE local_execution_r3_consumption_anchors SET anchor_id = ?1",
                    [Uuid::new_v4().to_string()],
                )
                .expect("substitute anchor id"),
            1
        );
        store
            .connection
            .execute_batch(LOCAL_EXECUTION_CONSUMPTION_ANCHORS_NO_UPDATE_TRIGGER)
            .expect("restore anchor guard");
        let counts: (i64, i64) = store
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM local_execution_r3_bridge_receipts),
                        (SELECT COUNT(*) FROM local_execution_r3_consumption_anchors)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("equal relation counts");
        assert_eq!(counts, (1, 1));
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
        assert_eq!(
            count(&database_path, "local_execution_r3_consumption_anchors"),
            1
        );

        let fixture = Fixture::new();
        let store = fixture.open();
        let database_path = store.database_path().to_owned();
        drop(store);
        let connection = Connection::open(&database_path).expect("forensic schema connection");
        connection
            .execute_batch("DROP TRIGGER local_execution_bridge_receipts_no_update;")
            .expect("remove one extension guard");
        drop(connection);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    fn assert_live_cursor_rejects_and_is_unchanged(store: &mut SyncStore, batch_id: Uuid) {
        assert!(matches!(
            store.commit_r3_change_batch(batch_id, 30),
            Err(Error::LocalMutationIncomplete)
        ));
        assert_eq!(
            store
                .vault_state()
                .expect("state after rejected cursor")
                .expect("bound state")
                .durable_cursor,
            Some("cursor-1".into()),
            "a rejected proof must leave the durable cursor unchanged"
        );
    }

    fn append_forged_mutation_event(
        database_path: &Path,
        operation_id: Uuid,
        attempt_number: u32,
        state_version: u64,
        evidence_id: Uuid,
    ) {
        Connection::open(database_path)
            .expect("forensic connection")
            .execute(
                "INSERT INTO mutation_events(
                    operation_id, attempt_number, state_version, phase, disposition,
                    evidence_id, outcome_code, occurred_at_unix_ms
                 ) VALUES (?1, ?2, ?3, 'completed', 'verified_applied', ?4,
                           'verified_applied', 21)",
                params![
                    operation_id.to_string(),
                    i64::from(attempt_number),
                    u64_to_i64(state_version).expect("test state version"),
                    evidence_id.to_string(),
                ],
            )
            .expect("append forged event permitted by the schema");
    }

    fn insert_forged_bridge_receipt(database_path: &Path, receipt: &BridgeReceiptFacts) {
        let fingerprint = bridge_receipt_fingerprint(receipt);
        Connection::open(database_path)
            .expect("forensic connection")
            .execute(
                "INSERT INTO local_execution_r3_bridge_receipts(
                    receipt_id, receipt_fingerprint, operation_id, attempt_number, boundary_id,
                    boundary_occurred_at_unix_ms, contract_fingerprint, outcome_id, evidence_id,
                    local_evidence_fingerprint, outcome_occurred_at_unix_ms,
                    r3_intent_fingerprint, r3_evidence_fingerprint, r3_outcome_code,
                    dependency_kind, r3_state_phase, r3_state_disposition, r3_attempt_number,
                    r3_state_version, r3_last_evidence_id, r3_event_state_version
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                           ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
                params![
                    bridge_receipt_id(fingerprint).to_string(),
                    fingerprint.as_slice(),
                    receipt.operation_id.to_string(),
                    i64::from(receipt.attempt_number),
                    receipt.boundary_id.to_string(),
                    u64_to_i64(receipt.boundary_occurred_at_unix_ms).expect("test timestamp"),
                    receipt.contract_fingerprint.as_slice(),
                    receipt.outcome_id.to_string(),
                    receipt.evidence_id.to_string(),
                    receipt.local_evidence_fingerprint.as_slice(),
                    u64_to_i64(receipt.outcome_occurred_at_unix_ms).expect("test timestamp"),
                    receipt.r3_intent_fingerprint,
                    receipt.r3_evidence_fingerprint,
                    receipt.r3_outcome_code,
                    receipt.dependency_kind,
                    receipt.r3_state_phase,
                    receipt.r3_state_disposition,
                    i64::from(receipt.r3_attempt_number),
                    u64_to_i64(receipt.r3_state_version).expect("test state version"),
                    receipt.r3_last_evidence_id.to_string(),
                    u64_to_i64(receipt.r3_event_state_version).expect("test state version"),
                ],
            )
            .expect("install forged receipt");
    }

    #[test]
    fn r3_5_coherent_local_proof_sqlite_forgery_cannot_advance_cursor_or_survive_reopen() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, intent, evidence, decision, dependency, batch_id) =
            bridged_r3_5_batch(&fixture);
        let original = bridge_receipt_for(
            &binding, &boundary, &intent, &evidence, &decision, dependency,
        );
        let mut forged = original.clone();
        forged.local_evidence_fingerprint = [0xa5; 32];
        forged.outcome_id = authoritative_outcome_id(
            boundary.operation_id,
            boundary.attempt_number,
            boundary.boundary_id,
            boundary.occurred_at_unix_ms,
            evidence.evidence_id,
            forged.local_evidence_fingerprint,
            LocalExecutionOutcome::VerifiedApplied,
            decision.recorded_at_unix_ms(),
        );
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER local_execution_outcomes_no_update", [])
            .expect("remove immutable outcome guard");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_update", [])
            .expect("remove immutable receipt guard");
        forensic
            .execute(
                "UPDATE local_execution_attempt_outcomes
                    SET outcome_id = ?1, evidence_fingerprint = ?2
                  WHERE operation_id = ?3 AND attempt_number = ?4",
                params![
                    forged.outcome_id.to_string(),
                    forged.local_evidence_fingerprint.as_slice(),
                    boundary.operation_id.to_string(),
                    i64::from(boundary.attempt_number),
                ],
            )
            .expect("coherent local outcome forgery");
        let forged_fingerprint = bridge_receipt_fingerprint(&forged);
        forensic
            .execute(
                "UPDATE local_execution_r3_bridge_receipts
                    SET receipt_id = ?1, receipt_fingerprint = ?2, outcome_id = ?3,
                        local_evidence_fingerprint = ?4
                  WHERE receipt_id = ?5",
                params![
                    bridge_receipt_id(forged_fingerprint).to_string(),
                    forged_fingerprint.as_slice(),
                    forged.outcome_id.to_string(),
                    forged.local_evidence_fingerprint.as_slice(),
                    bridge_receipt_id(bridge_receipt_fingerprint(&original)).to_string(),
                ],
            )
            .expect("coherent receipt forgery");
        forensic
            .execute_batch(LOCAL_EXECUTION_OUTCOMES_NO_UPDATE_TRIGGER)
            .expect("restore immutable outcome guard");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable receipt guard");
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
        assert_eq!(count(&database_path, "local_execution_attempt_outcomes"), 1);
        assert_eq!(
            count(&database_path, "local_execution_r3_bridge_receipts"),
            1
        );
    }

    #[test]
    fn r3_5_coherent_outcome_code_rewrite_cannot_advance_cursor_or_survive_reopen() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, intent, evidence, decision, dependency, batch_id) =
            bridged_r3_5_batch(&fixture);
        let original = bridge_receipt_for(
            &binding, &boundary, &intent, &evidence, &decision, dependency,
        );
        let mut forged = original.clone();
        forged.r3_outcome_code = Some("rewritten_outcome".into());
        let forged_fingerprint = bridge_receipt_fingerprint(&forged);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_events_no_update", [])
            .expect("remove immutable event guard");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_update", [])
            .expect("remove immutable receipt guard");
        forensic
            .execute(
                "UPDATE mutation_state SET outcome_code = ?1 WHERE operation_id = ?2",
                params!["rewritten_outcome", boundary.operation_id.to_string()],
            )
            .expect("rewrite state outcome code");
        forensic
            .execute(
                "UPDATE mutation_events SET outcome_code = ?1
                  WHERE operation_id = ?2 AND state_version = 2",
                params!["rewritten_outcome", boundary.operation_id.to_string()],
            )
            .expect("rewrite final event outcome code");
        forensic
            .execute(
                "UPDATE local_execution_r3_bridge_receipts
                    SET receipt_id = ?1, receipt_fingerprint = ?2, r3_outcome_code = ?3
                  WHERE receipt_id = ?4",
                params![
                    bridge_receipt_id(forged_fingerprint).to_string(),
                    forged_fingerprint.as_slice(),
                    "rewritten_outcome",
                    bridge_receipt_id(bridge_receipt_fingerprint(&original)).to_string(),
                ],
            )
            .expect("rewrite receipt outcome code and identity");
        forensic
            .execute_batch(MUTATION_EVENTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable event guard");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable receipt guard");
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_journal_seals_original_r3_evidence_against_coherent_rewrite() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, intent, evidence, decision, dependency, batch_id) =
            bridged_r3_5_batch(&fixture);
        let original = bridge_receipt_for(
            &binding, &boundary, &intent, &evidence, &decision, dependency,
        );
        let mut rewritten_evidence = evidence.clone();
        rewritten_evidence.outcome_code = Some("rewritten_outcome".into());
        rewritten_evidence.evidence_fingerprint = rewritten_evidence.canonical_fingerprint();
        let mut rewritten_receipt = original.clone();
        rewritten_receipt.r3_evidence_fingerprint = rewritten_evidence.evidence_fingerprint.clone();
        rewritten_receipt.r3_outcome_code = rewritten_evidence.outcome_code.clone();
        let rewritten_receipt_fingerprint = bridge_receipt_fingerprint(&rewritten_receipt);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        for trigger in [
            "mutation_evidence_no_update",
            "mutation_events_no_update",
            "local_execution_bridge_receipts_no_update",
        ] {
            forensic
                .execute(&format!("DROP TRIGGER {trigger}"), [])
                .expect("remove immutable guard");
        }
        forensic
            .execute(
                "UPDATE mutation_verification_evidence
                    SET outcome_code = ?1, evidence_fingerprint = ?2
                  WHERE evidence_id = ?3",
                params![
                    rewritten_evidence.outcome_code,
                    rewritten_evidence.evidence_fingerprint,
                    evidence.evidence_id.to_string(),
                ],
            )
            .expect("rewrite canonical R3 evidence preimage");
        forensic
            .execute(
                "UPDATE mutation_state SET outcome_code = ?1 WHERE operation_id = ?2",
                params!["rewritten_outcome", boundary.operation_id.to_string()],
            )
            .expect("rewrite state outcome code");
        forensic
            .execute(
                "UPDATE mutation_events SET outcome_code = ?1
                  WHERE operation_id = ?2 AND state_version = 2",
                params!["rewritten_outcome", boundary.operation_id.to_string()],
            )
            .expect("rewrite final event outcome code");
        forensic
            .execute(
                "UPDATE local_execution_r3_bridge_receipts
                    SET receipt_id = ?1, receipt_fingerprint = ?2,
                        r3_evidence_fingerprint = ?3, r3_outcome_code = ?4
                  WHERE receipt_id = ?5",
                params![
                    bridge_receipt_id(rewritten_receipt_fingerprint).to_string(),
                    rewritten_receipt_fingerprint.as_slice(),
                    rewritten_receipt.r3_evidence_fingerprint,
                    "rewritten_outcome",
                    bridge_receipt_id(bridge_receipt_fingerprint(&original)).to_string(),
                ],
            )
            .expect("rewrite coherent receipt identity");
        forensic
            .execute_batch(MUTATION_EVIDENCE_NO_UPDATE_TRIGGER)
            .expect("restore evidence guard");
        forensic
            .execute_batch(MUTATION_EVENTS_NO_UPDATE_TRIGGER)
            .expect("restore event guard");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER)
            .expect("restore receipt guard");
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_stale_or_semantically_invalid_evidence_rejects_live_and_on_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, _boundary, _, evidence, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_evidence_no_update", [])
            .expect("remove immutable evidence guard");
        forensic
            .execute(
                "UPDATE mutation_verification_evidence
                    SET observed_path = 'notes/tampered.md'
                  WHERE evidence_id = ?1",
                [evidence.evidence_id.to_string()],
            )
            .expect("leave stale fingerprint over changed preimage");
        forensic
            .execute_batch(MUTATION_EVIDENCE_NO_UPDATE_TRIGGER)
            .expect("restore immutable evidence guard");
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
        assert_eq!(count(&database_path, "mutation_verification_evidence"), 1);
        assert_eq!(count(&database_path, "local_execution_attempt_outcomes"), 1);
    }

    #[test]
    fn r3_5_self_canonical_but_intent_mismatched_evidence_rejects_live_and_on_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, _, _, evidence, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let mut forged = evidence.clone();
        forged.observed_path = Some("notes/not-the-bound-intent.md".into());
        forged.evidence_fingerprint = forged.canonical_fingerprint();
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_evidence_no_update", [])
            .expect("remove immutable evidence guard");
        forensic
            .execute(
                "UPDATE mutation_verification_evidence
                    SET observed_path = ?1, evidence_fingerprint = ?2
                  WHERE evidence_id = ?3",
                params![
                    forged.observed_path,
                    forged.evidence_fingerprint,
                    evidence.evidence_id.to_string(),
                ],
            )
            .expect("rewrite self-canonical but semantically mismatched evidence");
        forensic
            .execute_batch(MUTATION_EVIDENCE_NO_UPDATE_TRIGGER)
            .expect("restore immutable evidence guard");
        drop(forensic);
        assert!(!persisted_verified_applied_mutation_evidence_is_exact(
            &store.connection,
            evidence.evidence_id,
        )
        .expect("semantic recomputation"));

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_receipt_only_nullable_outcome_code_drift_is_not_hash_bypass() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, intent, evidence, decision, dependency, batch_id) =
            bridged_r3_5_batch(&fixture);
        let original = bridge_receipt_for(
            &binding, &boundary, &intent, &evidence, &decision, dependency,
        );
        let mut forged = original.clone();
        forged.r3_outcome_code = None;
        let forged_fingerprint = bridge_receipt_fingerprint(&forged);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_update", [])
            .expect("remove immutable receipt guard");
        forensic
            .execute(
                "UPDATE local_execution_r3_bridge_receipts
                    SET receipt_id = ?1, receipt_fingerprint = ?2, r3_outcome_code = NULL
                  WHERE receipt_id = ?3",
                params![
                    bridge_receipt_id(forged_fingerprint).to_string(),
                    forged_fingerprint.as_slice(),
                    bridge_receipt_id(bridge_receipt_fingerprint(&original)).to_string(),
                ],
            )
            .expect("rewrite nullable receipt field and hash");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable receipt guard");
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_clean_null_outcome_code_bridges_and_reopens_exactly() {
        let fixture = Fixture::new();
        let (mut store, _, _, _, evidence, _, _, batch_id) =
            bridged_r3_5_batch_with_outcome_code(&fixture, None);
        assert_eq!(evidence.outcome_code, None);
        store
            .commit_r3_change_batch(batch_id, 30)
            .expect("NULL outcome-code bridge cursor");
        drop(store);
        let reopened = fixture.open();
        assert_eq!(
            reopened
                .vault_state()
                .expect("state")
                .expect("bound")
                .durable_cursor,
            Some("cursor-2".into())
        );
    }

    #[test]
    fn r3_5_missing_journal_outcome_after_bridge_fails_closed_without_repair() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let outcome_path = fixture.journal_directory().join(format!(
            "{}-{}.out",
            boundary.operation_id, boundary.attempt_number
        ));
        fs::remove_file(&outcome_path).expect("remove journal outcome in forensic test");

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
        assert_eq!(count(&database_path, "local_execution_attempt_outcomes"), 1);
        assert_eq!(
            count(&database_path, "local_execution_r3_bridge_receipts"),
            1
        );
    }

    #[test]
    fn r3_5_missing_pre_after_bridge_rejects_live_and_reopen_without_repair() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let pre_path = fixture.journal_directory().join(format!(
            "{}-{}.pre",
            boundary.operation_id, boundary.attempt_number
        ));
        fs::remove_file(&pre_path).expect("remove journal pre in forensic test");

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::LocalExecutionJournalMismatch)
        ));
        assert_eq!(count(&database_path, "local_execution_attempt_outcomes"), 1);
        assert_eq!(
            count(&database_path, "local_execution_r3_bridge_receipts"),
            1
        );
    }

    #[test]
    fn r3_5_substituted_canonical_pre_rejects_even_when_out_embeds_original_pre() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let mut substituted = store
            .execution_journal
            .read_pre(boundary.operation_id, boundary.attempt_number)
            .expect("read original pre")
            .expect("pre exists");
        let embedded = store
            .execution_journal
            .read_outcome(boundary.operation_id, boundary.attempt_number)
            .expect("read original outcome")
            .expect("out exists");
        substituted.intent_fingerprint = [0x51; 32];
        assert_ne!(
            substituted, embedded.pre,
            "forensic pre must differ from out pre"
        );
        let pre_path = fixture.journal_directory().join(format!(
            "{}-{}.pre",
            boundary.operation_id, boundary.attempt_number
        ));
        fs::write(
            &pre_path,
            crate::sync_journal::canonical_pre_bytes_for_test(&substituted),
        )
        .expect("replace with canonical but substituted pre");
        make_private_file(&pre_path);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::LocalExecutionJournalMismatch)
        ));
    }

    #[test]
    fn r3_5_out_with_different_canonical_embedded_pre_rejects_live_and_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let pre = store
            .execution_journal
            .read_pre(boundary.operation_id, boundary.attempt_number)
            .expect("read pre")
            .expect("pre exists");
        let mut substituted_out = store
            .execution_journal
            .read_outcome(boundary.operation_id, boundary.attempt_number)
            .expect("read outcome")
            .expect("out exists");
        substituted_out.pre.collision_snapshot_fingerprint = [0x52; 32];
        assert_ne!(
            substituted_out.pre, pre,
            "out must embed a different canonical pre"
        );
        let out_path = fixture.journal_directory().join(format!(
            "{}-{}.out",
            boundary.operation_id, boundary.attempt_number
        ));
        fs::write(
            &out_path,
            crate::sync_journal::canonical_outcome_bytes_for_test(&substituted_out),
        )
        .expect("replace with canonical out containing substituted pre");
        make_private_file(&out_path);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_committed_dependency_receipt_deletion_cannot_downgrade_to_prereceipt() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_delete", [])
            .expect("remove receipt delete guard");
        forensic
            .execute(
                "DELETE FROM local_execution_r3_bridge_receipts
                  WHERE operation_id = ?1 AND attempt_number = ?2",
                params![
                    boundary.operation_id.to_string(),
                    i64::from(boundary.attempt_number)
                ],
            )
            .expect("delete committed bridge receipt");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_DELETE_TRIGGER)
            .expect("restore receipt delete guard");
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        assert_eq!(
            count(&database_path, "local_execution_r3_bridge_receipts"),
            0
        );
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_receipt_deletion_after_batch_cleanup_rejects_reopen_forever() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        store
            .commit_r3_change_batch(batch_id, 30)
            .expect("cursor finalization removes batch rows");
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_delete", [])
            .expect("remove immutable receipt guard");
        forensic
            .execute(
                "DELETE FROM local_execution_r3_bridge_receipts
                  WHERE operation_id = ?1 AND attempt_number = ?2",
                params![
                    boundary.operation_id.to_string(),
                    i64::from(boundary.attempt_number)
                ],
            )
            .expect("delete receipt after cascading batch cleanup");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_DELETE_TRIGGER)
            .expect("restore immutable receipt guard");
        drop(forensic);
        let marker_path = fixture.journal_directory().join(format!(
            "{}-{}.bridge",
            boundary.operation_id, boundary.attempt_number
        ));
        fs::remove_file(&marker_path).expect("remove marker with deleted receipt");
        assert_eq!(count(&database_path, "change_batch_mutations"), 0);
        assert_eq!(
            count(&database_path, "local_execution_r3_consumption_anchors"),
            1,
            "retained anchor prevents receipt+marker deletion from downgrading to pre-receipt"
        );
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_missing_or_substituted_consumption_marker_is_never_cursor_authority() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let marker_path = fixture.journal_directory().join(format!(
            "{}-{}.bridge",
            boundary.operation_id, boundary.attempt_number
        ));
        fs::remove_file(&marker_path).expect("remove bridge marker to model post-receipt crash");
        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        // A missing marker is the recoverable receipt-commit/publish crash
        // window, so opening remains forensic/retry-capable but not cursor
        // authoritative.
        let reopened = fixture.open();
        drop(reopened);

        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let marker_path = fixture.journal_directory().join(format!(
            "{}-{}.bridge",
            boundary.operation_id, boundary.attempt_number
        ));
        let mut marker = store
            .execution_journal
            .read_bridge_consumption(boundary.operation_id, boundary.attempt_number)
            .expect("read marker")
            .expect("marker exists");
        marker.r3_evidence_fingerprint = [0xa5; 32];
        fs::write(
            &marker_path,
            crate::sync_journal::canonical_bridge_consumption_bytes_for_test(&marker),
        )
        .expect("substitute canonical but wrong marker");
        make_private_file(&marker_path);
        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_marker_publish_interrupt_is_not_authority_until_reopen_sync_recovers() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, intent, evidence, decision, dependency, batch_id) =
            bridged_r3_5_batch(&fixture);
        let receipt = bridge_receipt_for(
            &binding, &boundary, &intent, &evidence, &decision, dependency,
        );
        let pre = store
            .execution_journal
            .read_pre(boundary.operation_id, boundary.attempt_number)
            .expect("read pre")
            .expect("pre exists");
        let consumption = bridge_consumption_witness(&pre, &receipt).expect("canonical marker");
        let marker_path = fixture.journal_directory().join(format!(
            "{}-{}.bridge",
            boundary.operation_id, boundary.attempt_number
        ));
        fs::remove_file(&marker_path).expect("model post-transaction marker interruption");
        store.execution_journal.fail_next_directory_sync_for_test();
        assert!(matches!(
            store
                .execution_journal
                .publish_bridge_consumption(&consumption),
            Err(Error::LocalExecutionJournalPublishedButNotSynced(_))
        ));
        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        // Cross the store/process lifetime without the bridge retry.  Open
        // itself performs the required directory fsync before it permits the
        // visible marker to become authority; this is deliberately different
        // from forgetting the live-process unconfirmed set.
        drop(store);
        let mut recovered = fixture.open();
        recovered
            .commit_r3_change_batch(batch_id, 30)
            .expect("reopen durability recovery establishes marker authority");
    }

    #[test]
    fn r3_5_pre_receipt_crash_boundary_recovers_without_cursor_or_bridge_authority() {
        let fixture = Fixture::new();
        let mut store = ready_store(&fixture);
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        store
            .register_local_execution_contract(&binding, 10)
            .expect("contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("boundary");
        store
            .publish_local_execution_pre_side_effect_witness(&binding, &boundary, 11)
            .expect("pre-side-effect witness");
        assert_eq!(
            store
                .inspect_local_execution_recovery(&binding, 0)
                .expect("recoverable crash boundary"),
            LocalExecutionRecoveryObservation::PreSideEffectWitnessOnly
        );
        assert_eq!(
            count(store.database_path(), "local_execution_attempt_outcomes"),
            0
        );
        assert_eq!(
            count(store.database_path(), "local_execution_r3_bridge_receipts"),
            0
        );
        assert_eq!(
            store
                .vault_state()
                .expect("state")
                .expect("bound")
                .durable_cursor,
            Some("cursor-1".into())
        );
        drop(store);
        let reopened = fixture.open();
        assert_eq!(
            reopened
                .inspect_local_execution_recovery(&binding, 0)
                .expect("reopen recovery"),
            LocalExecutionRecoveryObservation::PreSideEffectWitnessOnly
        );
        assert_eq!(
            reopened
                .vault_state()
                .expect("state")
                .expect("bound")
                .durable_cursor,
            Some("cursor-1".into())
        );
    }

    #[test]
    fn r3_5_stale_base_reference_fingerprint_rejects_live_and_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_intents_no_update", [])
            .expect("remove intent guard");
        forensic
            .execute(
                "UPDATE mutation_intents SET base_reference = 'base-reference-a'
                  WHERE operation_id = ?1",
                [boundary.operation_id.to_string()],
            )
            .expect("alter valid base reference without matching fingerprint");
        forensic
            .execute_batch(MUTATION_INTENTS_NO_UPDATE_TRIGGER)
            .expect("restore exact intent guard");
        let persisted = load_persisted_mutation_intent(&forensic, boundary.operation_id)
            .expect("read full persisted intent")
            .expect("intent exists");
        assert!(matches!(
            validate_mutation_intent(&persisted),
            Err(Error::InvalidTransferEvidence)
        ));
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_stale_local_object_id_fingerprint_rejects_live_and_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_intents_no_update", [])
            .expect("remove intent guard");
        forensic
            .execute(
                "UPDATE mutation_intents SET local_object_id = 'local-object-a'
                  WHERE operation_id = ?1",
                [boundary.operation_id.to_string()],
            )
            .expect("alter valid local object without matching fingerprint");
        forensic
            .execute_batch(MUTATION_INTENTS_NO_UPDATE_TRIGGER)
            .expect("restore exact intent guard");
        let persisted = load_persisted_mutation_intent(&forensic, boundary.operation_id)
            .expect("read full persisted intent")
            .expect("intent exists");
        assert!(matches!(
            validate_mutation_intent(&persisted),
            Err(Error::InvalidTransferEvidence)
        ));
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_direct_intent_reregistration_rejects_stale_and_coherent_rewrites() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, intent, _, _, _, _) = bridged_r3_5_batch(&fixture);
        let forensic = Connection::open(store.database_path()).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_intents_no_update", [])
            .expect("remove immutable guard");
        forensic
            .execute(
                "UPDATE mutation_intents SET base_reference = 'stale-base' WHERE operation_id = ?1",
                [boundary.operation_id.to_string()],
            )
            .expect("stale rewrite");
        forensic
            .execute_batch(MUTATION_INTENTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable guard");
        drop(forensic);
        assert!(matches!(
            store.register_mutation_intent(&intent, None),
            Err(Error::MutationCollision)
        ));

        let fixture = Fixture::new();
        let (mut store, _, boundary, intent, _, _, _, _) = bridged_r3_5_batch(&fixture);
        let forensic = Connection::open(store.database_path()).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_intents_no_update", [])
            .expect("remove immutable guard");
        let mut rewritten = load_persisted_mutation_intent(&forensic, boundary.operation_id)
            .expect("read intent")
            .expect("intent exists");
        rewritten.base_reference = Some("coherent-base".into());
        rewritten.local_object_id = Some("coherent-local".into());
        rewritten.intent_fingerprint = rewritten.canonical_fingerprint();
        forensic
            .execute(
                "UPDATE mutation_intents SET base_reference = ?1, local_object_id = ?2,
                    intent_fingerprint = ?3 WHERE operation_id = ?4",
                params![
                    rewritten.base_reference,
                    rewritten.local_object_id,
                    rewritten.intent_fingerprint,
                    boundary.operation_id.to_string(),
                ],
            )
            .expect("coherent rewrite");
        forensic
            .execute_batch(MUTATION_INTENTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable guard");
        drop(forensic);
        assert!(matches!(
            store.register_mutation_intent(&intent, None),
            Err(Error::MutationCollision)
        ));
    }

    #[test]
    fn r3_5_operation_scoped_reregistration_ignores_corrupt_b_but_reopen_rejects_it() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let a = test_durable_execution_binding(Uuid::new_v4(), fixture.vault_id);
        let b = test_durable_execution_binding(Uuid::new_v4(), fixture.vault_id);
        store
            .register_local_execution_contract(&a, 10)
            .expect("healthy A contract");
        store
            .register_local_execution_contract(&b, 10)
            .expect("B contract");
        let forensic = Connection::open(store.database_path()).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER local_execution_contracts_no_update", [])
            .expect("remove immutable guard");
        forensic
            .execute(
                "UPDATE local_execution_contracts SET target_name = 'corrupt-b'
                  WHERE operation_id = ?1",
                [b.persistence_projection().operation_id.to_string()],
            )
            .expect("corrupt unrelated B contract");
        forensic
            .execute_batch(LOCAL_EXECUTION_CONTRACTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable guard");
        drop(forensic);
        assert!(matches!(
            store.register_local_execution_contract(&a, 10),
            Ok(LocalExecutionRegistrationOutcome::AlreadyPresent)
        ));
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_history_rejects_running_after_final_time_and_attempt_jump() {
        for (column, value) in [("occurred_at_unix_ms", "22"), ("attempt_number", "1")] {
            let fixture = Fixture::new();
            let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
            let forensic = Connection::open(store.database_path()).expect("forensic connection");
            forensic
                .execute("DROP TRIGGER mutation_events_no_update", [])
                .expect("remove immutable event guard");
            forensic
                .execute(
                    &format!(
                        "UPDATE mutation_events SET {column} = {value}
                          WHERE operation_id = ?1 AND state_version = 1"
                    ),
                    [boundary.operation_id.to_string()],
                )
                .expect("tamper intermediate running event");
            forensic
                .execute_batch(MUTATION_EVENTS_NO_UPDATE_TRIGGER)
                .expect("restore immutable event guard");
            drop(forensic);
            assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
            drop(store);
            assert!(matches!(
                SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
                Err(Error::InvalidSchema)
            ));
        }
    }

    #[test]
    fn r3_5_coherent_full_intent_rewrite_rejects_against_unchanged_local_contract_and_receipt() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_intents_no_update", [])
            .expect("remove intent guard");
        let mut rewritten = load_persisted_mutation_intent(&forensic, boundary.operation_id)
            .expect("read full persisted intent")
            .expect("intent exists");
        rewritten.base_reference = Some("base-reference-a".into());
        rewritten.local_object_id = Some("local-object-a".into());
        rewritten.intent_fingerprint = rewritten.canonical_fingerprint();
        validate_mutation_intent(&rewritten).expect("coherent full intent rewrite");
        forensic
            .execute(
                "UPDATE mutation_intents
                    SET base_reference = ?1, local_object_id = ?2, intent_fingerprint = ?3
                  WHERE operation_id = ?4",
                params![
                    rewritten.base_reference,
                    rewritten.local_object_id,
                    rewritten.intent_fingerprint,
                    boundary.operation_id.to_string(),
                ],
            )
            .expect("rewrite all changed fields and fingerprint");
        forensic
            .execute_batch(MUTATION_INTENTS_NO_UPDATE_TRIGGER)
            .expect("restore exact intent guard");
        let persisted = load_persisted_mutation_intent(&forensic, boundary.operation_id)
            .expect("read rewritten full intent")
            .expect("intent exists");
        validate_mutation_intent(&persisted).expect("full typed intent validation reached");
        assert_ne!(
            binding.persistence_projection().intent_fingerprint,
            local_intent_fingerprint_from_r3_intent(&persisted.intent_fingerprint)
                .expect("derived changed local fingerprint"),
            "unchanged local contract must bind the original R3 intent"
        );
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_event_zero_semantic_tamper_with_versions_zero_one_two_rejects_live_and_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_events_no_update", [])
            .expect("remove event guard");
        forensic
            .execute(
                "UPDATE mutation_events
                    SET phase = 'running'
                  WHERE operation_id = ?1 AND state_version = 0",
                [boundary.operation_id.to_string()],
            )
            .expect("semantic tamper of initial event");
        forensic
            .execute_batch(MUTATION_EVENTS_NO_UPDATE_TRIGGER)
            .expect("restore exact event guard");
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_event_one_semantic_tamper_with_versions_zero_one_two_rejects_live_and_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_events_no_update", [])
            .expect("remove event guard");
        forensic
            .execute(
                "UPDATE mutation_events
                    SET phase = 'needs_reconcile', disposition = 'needs_reconcile'
                  WHERE operation_id = ?1 AND state_version = 1",
                [boundary.operation_id.to_string()],
            )
            .expect("semantic tamper of non-final event");
        forensic
            .execute_batch(MUTATION_EVENTS_NO_UPDATE_TRIGGER)
            .expect("restore exact event guard");
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_missing_middle_event_rejects_live_and_reopen_without_advancing_cursor() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, _, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_events_no_delete", [])
            .expect("remove event delete guard");
        forensic
            .execute(
                "DELETE FROM mutation_events WHERE operation_id = ?1 AND state_version = 1",
                [boundary.operation_id.to_string()],
            )
            .expect("delete middle event under forensic setup");
        forensic
            .execute_batch(MUTATION_EVENTS_NO_DELETE_TRIGGER)
            .expect("restore exact event delete guard");
        drop(forensic);

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn retry_attempt_overflow_fails_closed_before_state_event_or_evidence_artifacts() {
        let fixture = Fixture::new();
        let mut store = ready_store(&fixture);
        let operation_id = Uuid::new_v4();
        let intent = local_publish_intent(operation_id);
        store
            .register_mutation_intent(&intent, None)
            .expect("intent durable");
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute(
                "UPDATE mutation_state
                    SET phase = 'retry_scheduled', attempt_number = ?1, state_version = 1,
                        next_attempt_at_unix_ms = 12, retry_mode = 'restart_exact',
                        resume_reference = NULL, updated_at_unix_ms = 12
                  WHERE operation_id = ?2",
                params![i64::from(u32::MAX), operation_id.to_string()],
            )
            .expect("forge due retry at maximum attempt");
        forensic
            .execute(
                "INSERT INTO mutation_events(
                    operation_id, attempt_number, state_version, phase, disposition,
                    evidence_id, outcome_code, occurred_at_unix_ms
                 ) VALUES (?1, ?2, 1, 'retry_scheduled', NULL, NULL, NULL, 12)",
                params![operation_id.to_string(), i64::from(u32::MAX)],
            )
            .expect("preserve append-only forensic history shape");
        drop(forensic);
        let state_before = store
            .mutation_state(operation_id)
            .expect("read forged retry state")
            .expect("state exists");
        let events_before = count(&database_path, "mutation_events");
        let evidence_before = count(&database_path, "mutation_verification_evidence");

        let result = catch_unwind(AssertUnwindSafe(|| {
            store.claim_mutation(operation_id, 1, 12)
        }));
        assert!(
            matches!(result, Ok(Err(Error::InvalidSchema))),
            "{result:?}"
        );
        assert_eq!(
            store
                .mutation_state(operation_id)
                .expect("read state after overflow"),
            Some(state_before)
        );
        assert_eq!(count(&database_path, "mutation_events"), events_before);
        assert_eq!(
            count(&database_path, "mutation_verification_evidence"),
            evidence_before
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn r3_5_authoritative_finalization_matrix_is_exact_idempotent_and_restart_safe() {
        for (index, (requested, call_fact, expected_non_retryable)) in [
            (
                LocalExecutionOutcome::VerifiedApplied,
                PlatformCallFact::Returned,
                false,
            ),
            (
                LocalExecutionOutcome::VerifiedNotApplied,
                PlatformCallFact::NotEntered,
                false,
            ),
            (
                LocalExecutionOutcome::WriteOutcomeUnknown,
                PlatformCallFact::Ambiguous,
                true,
            ),
            (
                LocalExecutionOutcome::NeedsReconcile,
                PlatformCallFact::Returned,
                true,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let (fixture, mut store, binding, boundary) = prepared_journal_attempt();
            store
                .publish_local_execution_pre_side_effect_witness(&binding, &boundary, 11)
                .expect("pre witness");
            let decision = classify_authoritative_final_outcome(
                &binding,
                &boundary,
                test_authoritative_evidence_with_identity(
                    &binding,
                    &boundary,
                    call_fact,
                    requested,
                    Uuid::new_v5(
                        &Uuid::NAMESPACE_OID,
                        format!("r3.5-finalization-matrix-{index}").as_bytes(),
                    ),
                    [u8::try_from(index + 1).expect("small matrix index"); 32],
                ),
            )
            .expect("classify authoritative outcome");
            assert_eq!(decision.outcome(), requested);
            assert_eq!(
                store
                    .finalize_authoritative_local_execution_outcome(&binding, &boundary, &decision)
                    .expect("first finalization"),
                LocalExecutionRegistrationOutcome::Registered
            );
            assert_eq!(
                store
                    .finalize_authoritative_local_execution_outcome(&binding, &boundary, &decision)
                    .expect("idempotent finalization"),
                LocalExecutionRegistrationOutcome::AlreadyPresent
            );
            let pre = store
                .execution_journal
                .read_pre(boundary.operation_id, boundary.attempt_number)
                .expect("read journal pre")
                .expect("journal pre exists");
            let out = store
                .execution_journal
                .read_outcome(boundary.operation_id, boundary.attempt_number)
                .expect("read journal out")
                .expect("journal out exists");
            assert_eq!(out.pre, pre, "exact named and embedded pre pair");
            assert_eq!(out.outcome_id, decision.outcome_id());
            assert_eq!(out.evidence_id, decision.evidence_id());
            assert_eq!(out.outcome, requested);
            assert_eq!(
                out.r3_mutation_evidence_fingerprint,
                Some(decision.r3_mutation_evidence_fingerprint())
            );
            let ledger = store
                .local_execution_attempt_outcome(boundary.operation_id, boundary.attempt_number)
                .expect("read local ledger")
                .expect("local ledger exists");
            assert_eq!(ledger.outcome_id, decision.outcome_id());
            assert_eq!(ledger.non_retryable, expected_non_retryable);
            assert_eq!(
                count(store.database_path(), "local_execution_r3_bridge_receipts"),
                0,
                "finalization alone never creates bridge authority"
            );
            if requested != LocalExecutionOutcome::VerifiedApplied {
                assert!(matches!(
                    store.commit_r3_5_verified_local_execution_dependency(
                        Uuid::new_v4(),
                        ChangeBatchDependency {
                            operation_id: boundary.operation_id,
                            kind: ChangeBatchDependencyKind::Mutation,
                        },
                        &binding,
                        boundary.attempt_number,
                        &decision,
                    ),
                    Err(Error::LocalMutationIncomplete)
                ));
                assert_eq!(
                    count(store.database_path(), "local_execution_r3_bridge_receipts"),
                    0,
                    "non-applied final outcome cannot create a receipt"
                );
            }
            drop(store);
            let reopened = fixture.open();
            assert!(matches!(
                reopened.inspect_local_execution_recovery(&binding, boundary.attempt_number),
                Ok(LocalExecutionRecoveryObservation::OutcomeWitnessAndLedgerMatch { .. })
            ));
            assert_eq!(
                reopened
                    .local_execution_attempt_outcome(boundary.operation_id, boundary.attempt_number)
                    .expect("read reopen ledger")
                    .expect("reopen ledger exists")
                    .outcome_id,
                decision.outcome_id()
            );
        }
    }

    #[test]
    fn r3_5_exact_reregistration_with_rotated_attestation_remains_already_present() {
        let fixture = Fixture::new();
        let operation_id = Uuid::new_v4();
        let original = test_durable_execution_binding(operation_id, fixture.vault_id);
        let rotated = test_durable_execution_binding_with_attestation_offset(
            operation_id,
            fixture.vault_id,
            9,
        );
        let mut store = fixture.open();
        assert_eq!(
            store
                .register_local_execution_contract(&original, 10)
                .expect("first registration"),
            LocalExecutionRegistrationOutcome::Registered
        );
        assert_eq!(
            store
                .register_local_execution_contract(&rotated, 10)
                .expect("rotated exact re-registration"),
            LocalExecutionRegistrationOutcome::AlreadyPresent
        );
    }

    #[test]
    fn r3_5_reregistration_rejects_structurally_valid_child_row_tampering_immediately() {
        enum ChildTamper {
            Identity,
            CollisionMember,
            Completion,
        }
        for tamper in [
            ChildTamper::Identity,
            ChildTamper::CollisionMember,
            ChildTamper::Completion,
        ] {
            let fixture = Fixture::new();
            let operation_id = Uuid::new_v4();
            let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
            let mut store = fixture.open();
            store
                .register_local_execution_contract(&binding, 10)
                .expect("first registration");
            let forensic = Connection::open(store.database_path()).expect("forensic connection");
            match tamper {
                ChildTamper::Identity => {
                    forensic
                        .execute("DROP TRIGGER local_execution_identities_no_update", [])
                        .expect("remove identity guard");
                    forensic
                        .execute(
                            "UPDATE local_execution_identity_evidence
                                SET provider_id = X'01'
                              WHERE operation_id = ?1 AND role = 'vault_root'",
                            [operation_id.to_string()],
                        )
                        .expect("structurally valid identity rewrite");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_IDENTITIES_NO_UPDATE_TRIGGER)
                        .expect("restore identity guard");
                }
                ChildTamper::CollisionMember => {
                    forensic
                        .execute("DROP TRIGGER local_execution_members_no_update", [])
                        .expect("remove member guard");
                    forensic
                        .execute(
                            "UPDATE local_execution_collision_members
                                SET collision_key = 'different'
                              WHERE operation_id = ?1 AND ordinal = 0",
                            [operation_id.to_string()],
                        )
                        .expect("structurally valid collision rewrite");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_MEMBERS_NO_UPDATE_TRIGGER)
                        .expect("restore member guard");
                }
                ChildTamper::Completion => {
                    forensic
                        .execute("DROP TRIGGER local_execution_completions_no_update", [])
                        .expect("remove completion guard");
                    forensic
                        .execute(
                            "UPDATE local_execution_contract_completions
                                SET completed_at_unix_ms = 11
                              WHERE operation_id = ?1",
                            [operation_id.to_string()],
                        )
                        .expect("structurally valid completion rewrite");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_COMPLETIONS_NO_UPDATE_TRIGGER)
                        .expect("restore completion guard");
                }
            }
            drop(forensic);
            assert!(matches!(
                store.register_local_execution_contract(&binding, 10),
                Err(Error::LocalExecutionJournalMismatch)
            ));
        }
    }

    #[test]
    fn mutation_state_version_max_fails_closed_before_evidence_or_event_write() {
        let fixture = Fixture::new();
        let mut store = ready_store(&fixture);
        let operation_id = Uuid::new_v4();
        let intent = local_publish_intent(operation_id);
        store
            .register_mutation_intent(&intent, None)
            .expect("intent");
        store.claim_mutation(operation_id, 0, 12).expect("claim");
        let evidence = local_publish_evidence(Uuid::new_v4(), &intent);
        let database_path = store.database_path().to_owned();
        Connection::open(&database_path)
            .expect("forensic connection")
            .execute(
                "UPDATE mutation_state SET state_version = ?1 WHERE operation_id = ?2",
                params![i64::MAX, operation_id.to_string()],
            )
            .expect("set adversarial state version");
        let evidence_count_before = count(&database_path, "mutation_verification_evidence");
        let event_count_before = count(&database_path, "mutation_events");
        let result = catch_unwind(AssertUnwindSafe(|| {
            store.record_mutation_outcome(
                operation_id,
                i64::MAX as u64,
                &evidence,
                &MutationOutcomeTransition::VerifiedApplied,
            )
        }));
        assert!(
            matches!(
                result,
                Ok(Err(Error::InvalidSchema | Error::InvalidTimestamp))
            ),
            "unexpected max-version result: {result:?}"
        );
        assert_eq!(
            count(&database_path, "mutation_verification_evidence"),
            evidence_count_before
        );
        assert_eq!(count(&database_path, "mutation_events"), event_count_before);
    }

    #[test]
    fn r3_5_future_mutation_event_cannot_advance_cursor_or_survive_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, evidence, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        append_forged_mutation_event(
            &database_path,
            boundary.operation_id,
            0,
            3,
            evidence.evidence_id,
        );

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_i64_max_state_event_and_receipt_rewrite_fails_closed_without_panicking() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, intent, evidence, decision, dependency, batch_id) =
            bridged_r3_5_batch(&fixture);
        let original = bridge_receipt_for(
            &binding, &boundary, &intent, &evidence, &decision, dependency,
        );
        let mut forged = original.clone();
        forged.r3_state_version = i64::MAX as u64;
        forged.r3_event_state_version = i64::MAX as u64;
        let forged_fingerprint = bridge_receipt_fingerprint(&forged);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_events_no_update", [])
            .expect("remove immutable event guard");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_update", [])
            .expect("remove immutable receipt guard");
        forensic
            .execute(
                "UPDATE mutation_state SET state_version = ?1 WHERE operation_id = ?2",
                params![i64::MAX, boundary.operation_id.to_string()],
            )
            .expect("rewrite state version to i64 max");
        forensic
            .execute(
                "UPDATE mutation_events SET state_version = ?1
                  WHERE operation_id = ?2 AND state_version = 2",
                params![i64::MAX, boundary.operation_id.to_string()],
            )
            .expect("rewrite final event version to i64 max");
        forensic
            .execute(
                "UPDATE local_execution_r3_bridge_receipts
                    SET receipt_id = ?1, receipt_fingerprint = ?2,
                        r3_state_version = ?3, r3_event_state_version = ?3
                  WHERE receipt_id = ?4",
                params![
                    bridge_receipt_id(forged_fingerprint).to_string(),
                    forged_fingerprint.as_slice(),
                    i64::MAX,
                    bridge_receipt_id(bridge_receipt_fingerprint(&original)).to_string(),
                ],
            )
            .expect("rewrite receipt versions and identity");
        forensic
            .execute_batch(MUTATION_EVENTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable event guard");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable receipt guard");
        drop(forensic);

        let result = catch_unwind(AssertUnwindSafe(|| {
            store.commit_r3_change_batch(batch_id, 30)
        }));
        assert!(matches!(result, Ok(Err(Error::LocalMutationIncomplete))));
        assert_eq!(
            store
                .vault_state()
                .expect("state")
                .expect("bound")
                .durable_cursor,
            Some("cursor-1".into())
        );
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_duplicate_mutation_state_version_in_same_attempt_is_rejected_live_and_on_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, evidence, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        append_forged_mutation_event(
            &database_path,
            boundary.operation_id,
            0,
            2,
            evidence.evidence_id,
        );

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_duplicate_mutation_state_version_across_attempts_is_rejected_live_and_on_reopen() {
        let fixture = Fixture::new();
        let (mut store, _, boundary, _, evidence, _, _, batch_id) = bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        append_forged_mutation_event(
            &database_path,
            boundary.operation_id,
            1,
            2,
            evidence.evidence_id,
        );

        assert_live_cursor_rejects_and_is_unchanged(&mut store, batch_id);
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn r3_5_canonical_unreceipted_outcome_and_exact_journal_reopen_cleanly() {
        let (fixture, mut store, binding, boundary) = prepared_journal_attempt();
        store
            .publish_local_execution_pre_side_effect_witness(&binding, &boundary, 11)
            .expect("pre-side-effect witness");
        let decision = classify_authoritative_final_outcome(
            &binding,
            &boundary,
            test_authoritative_evidence(
                &binding,
                &boundary,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
            ),
        )
        .expect("authoritative decision");
        store
            .finalize_authoritative_local_execution_outcome(&binding, &boundary, &decision)
            .expect("canonical unreceipted outcome");
        drop(store);

        let reopened = fixture.open();
        assert!(matches!(
            reopened.inspect_local_execution_recovery(&binding, boundary.attempt_number),
            Ok(LocalExecutionRecoveryObservation::OutcomeWitnessAndLedgerMatch { .. })
        ));
    }

    #[test]
    fn r3_5_arbitrary_deterministic_id_on_unreceipted_outcome_is_rejected_on_reopen() {
        let (fixture, mut store, binding, boundary) = prepared_journal_attempt();
        store
            .publish_local_execution_pre_side_effect_witness(&binding, &boundary, 11)
            .expect("pre-side-effect witness");
        let decision = classify_authoritative_final_outcome(
            &binding,
            &boundary,
            test_authoritative_evidence(
                &binding,
                &boundary,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
            ),
        )
        .expect("authoritative decision");
        store
            .finalize_authoritative_local_execution_outcome(&binding, &boundary, &decision)
            .expect("canonical unreceipted outcome");
        let database_path = store.database_path().to_owned();
        let forged_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, b"forged-unreceipted-outcome-id");
        assert_ne!(forged_id, decision.outcome_id());
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER local_execution_outcomes_no_update", [])
            .expect("remove immutable outcome guard");
        forensic
            .execute(
                "UPDATE local_execution_attempt_outcomes SET outcome_id = ?1
                  WHERE operation_id = ?2 AND attempt_number = ?3",
                params![
                    forged_id.to_string(),
                    boundary.operation_id.to_string(),
                    i64::from(boundary.attempt_number),
                ],
            )
            .expect("install arbitrary deterministic id");
        forensic
            .execute_batch(LOCAL_EXECUTION_OUTCOMES_NO_UPDATE_TRIGGER)
            .expect("restore immutable outcome guard");
        drop(forensic);
        drop(store);

        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
        assert_eq!(count(&database_path, "local_execution_attempt_outcomes"), 1);
    }

    #[test]
    fn r3_5_contract_first_bridged_restart_exact_binding_and_timestamp_is_already_present() {
        let fixture = Fixture::new();
        let (store, binding, boundary, _, _, _, _, _) = bridged_r3_5_batch(&fixture);
        let r3_state = store
            .mutation_state(boundary.operation_id)
            .expect("R3 state")
            .expect("completed R3 state");
        assert_eq!(r3_state.phase, MutationPhase::Completed);
        assert_eq!(r3_state.attempt_number, 0);
        assert_eq!(r3_state.state_version, 2);
        assert_eq!(
            r3_state.disposition,
            Some(MutationDisposition::VerifiedApplied)
        );
        drop(store);

        let mut reopened = fixture.open();
        assert_eq!(
            reopened
                .register_local_execution_contract(&binding, 10)
                .expect("same original registration timestamp"),
            LocalExecutionRegistrationOutcome::AlreadyPresent
        );
        assert_eq!(
            count(
                reopened.database_path(),
                "local_execution_r3_bridge_receipts"
            ),
            1
        );
    }

    #[test]
    fn r3_5_receipt_collision_rejects_bridge_atomically_and_preserves_dependency_and_cursor() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, intent, evidence, decision, dependency, batch_id) =
            bridged_r3_5_batch(&fixture);
        let database_path = store.database_path().to_owned();
        let original = bridge_receipt_for(
            &binding, &boundary, &intent, &evidence, &decision, dependency,
        );
        let mut collision = original.clone();
        collision.r3_outcome_code = Some("receipt_collision".into());
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_delete", [])
            .expect("remove immutable receipt delete guard");
        forensic
            .execute(
                "DELETE FROM local_execution_r3_bridge_receipts WHERE receipt_id = ?1",
                [bridge_receipt_id(bridge_receipt_fingerprint(&original)).to_string()],
            )
            .expect("remove original receipt for collision setup");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_DELETE_TRIGGER)
            .expect("restore immutable receipt delete guard");
        drop(forensic);
        insert_forged_bridge_receipt(&database_path, &collision);
        assert_eq!(
            count(&database_path, "local_execution_r3_bridge_receipts"),
            1
        );

        assert!(store
            .commit_r3_5_verified_local_execution_dependency(
                batch_id, dependency, &binding, 0, &decision,
            )
            .is_err());
        assert_eq!(
            count(&database_path, "local_execution_r3_bridge_receipts"),
            1
        );
        assert_eq!(
            store
                .active_change_batch()
                .expect("active batch")
                .expect("batch")
                .committed_mutations,
            1,
            "the already-committed dependency must remain unchanged"
        );
        assert_eq!(
            store
                .vault_state()
                .expect("state")
                .expect("bound")
                .durable_cursor,
            Some("cursor-1".into())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn r3_5_bridge_requires_cross_layer_exact_verified_applied_proof() {
        let fixture = Fixture::new();
        let mut store = ready_store(&fixture);
        let operation_id = Uuid::new_v4();
        let intent = local_publish_intent(operation_id);
        let binding = test_durable_execution_binding_for_r3_intent(
            operation_id,
            fixture.vault_id,
            &intent.intent_fingerprint,
        );
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        store
            .register_local_execution_contract(&binding, 10)
            .expect("contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("boundary");
        store
            .publish_local_execution_pre_side_effect_witness(&binding, &boundary, 11)
            .expect("pre witness");
        store
            .register_mutation_intent(&intent, None)
            .expect("intent");
        store.claim_mutation(operation_id, 0, 12).expect("claim");
        let evidence_id = Uuid::new_v4();
        let r3_evidence = local_publish_evidence(evidence_id, &intent);
        let evidence_fingerprint = parse_canonical_sha256(&r3_evidence.evidence_fingerprint)
            .expect("canonical fingerprint");
        store
            .record_mutation_outcome(
                operation_id,
                1,
                &r3_evidence,
                &MutationOutcomeTransition::VerifiedApplied,
            )
            .expect("r3 completion");
        let dependency = ChangeBatchDependency {
            operation_id,
            kind: ChangeBatchDependencyKind::Mutation,
        };
        let batch_id = Uuid::new_v4();
        store
            .begin_r3_change_batch(
                batch_id,
                "cursor-1",
                "cursor-2",
                std::slice::from_ref(&dependency),
            )
            .expect("batch");
        let decision = classify_authoritative_final_outcome(
            &binding,
            &boundary,
            test_authoritative_evidence_with_identity(
                &binding,
                &boundary,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
                evidence_id,
                evidence_fingerprint,
            ),
        )
        .expect("test evidence");
        store
            .finalize_authoritative_local_execution_outcome(&binding, &boundary, &decision)
            .expect("local finalization");
        // A durable v6 contract closes the legacy public cursor gate even
        // when every legacy R3 row is otherwise exact.
        assert!(matches!(
            store.commit_r3_change_dependency(batch_id, dependency, evidence_id),
            Err(Error::LocalMutationIncomplete)
        ));
        // The receipt and public dependency are one SQLite transaction.  A
        // fault after receipt insertion must leave neither a receipt nor a
        // committed dependency behind.
        Connection::open(store.database_path())
            .expect("fault connection")
            .execute_batch(
                "CREATE TRIGGER fault_after_bridge_receipt
                 BEFORE UPDATE ON change_batch_mutations
                 BEGIN SELECT RAISE(ABORT, 'injected fault'); END;",
            )
            .expect("install dependency-update fault");
        assert!(store
            .commit_r3_5_verified_local_execution_dependency(
                batch_id, dependency, &binding, 0, &decision,
            )
            .is_err());
        assert_eq!(
            count(store.database_path(), "local_execution_r3_bridge_receipts"),
            0
        );
        assert_eq!(
            store
                .active_change_batch()
                .expect("active batch")
                .expect("batch")
                .committed_mutations,
            0
        );
        Connection::open(store.database_path())
            .expect("fault connection")
            .execute("DROP TRIGGER fault_after_bridge_receipt", [])
            .expect("restore exact trigger family");
        store
            .commit_r3_5_verified_local_execution_dependency(
                batch_id, dependency, &binding, 0, &decision,
            )
            .expect("bridge");
        assert_eq!(
            count(store.database_path(), "local_execution_r3_bridge_receipts"),
            1
        );
        // The receipt carries the sealed local/R3 relation across a process
        // restart; the cursor gate revalidates it rather than reconstructing
        // authority from journal or caller input.
        drop(store);
        let mut store = fixture.open();
        assert!(matches!(
            store.commit_r3_5_verified_local_execution_dependency(
                batch_id, dependency, &binding, 0, &decision,
            ),
            Ok(())
        ));
        assert_eq!(
            count(store.database_path(), "local_execution_r3_bridge_receipts"),
            1
        );
        assert!(store
            .commit_r3_5_verified_local_execution_dependency(
                batch_id, dependency, &binding, 1, &decision,
            )
            .is_err());
        // A self-consistent receipt/ledger pair is not authority: changing
        // the deterministic local outcome identifier must reach its canonical
        // recomputation, rather than failing at receipt↔ledger equality.
        let original_receipt = BridgeReceiptFacts {
            operation_id,
            attempt_number: 0,
            boundary_id: boundary.boundary_id,
            boundary_occurred_at_unix_ms: boundary.occurred_at_unix_ms,
            contract_fingerprint: *binding.fingerprint().as_bytes(),
            outcome_id: decision.outcome_id(),
            evidence_id,
            local_evidence_fingerprint: decision.evidence_fingerprint(),
            outcome_occurred_at_unix_ms: decision.recorded_at_unix_ms(),
            r3_intent_fingerprint: intent.intent_fingerprint.clone(),
            r3_evidence_fingerprint: r3_evidence.evidence_fingerprint.clone(),
            r3_outcome_code: r3_evidence.outcome_code.clone(),
            dependency_kind: ChangeBatchDependencyKind::Mutation.as_str().to_owned(),
            r3_state_phase: "completed".to_owned(),
            r3_state_disposition: "verified_applied".to_owned(),
            r3_attempt_number: 0,
            r3_state_version: 2,
            r3_last_evidence_id: evidence_id,
            r3_event_state_version: 2,
        };
        let original_fingerprint = bridge_receipt_fingerprint(&original_receipt);
        let mut tampered_receipt = original_receipt.clone();
        tampered_receipt.outcome_id = Uuid::new_v4();
        let tampered_fingerprint = bridge_receipt_fingerprint(&tampered_receipt);
        let tampered_id = bridge_receipt_id(tampered_fingerprint);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER local_execution_outcomes_no_update", [])
            .expect("temporarily remove local outcome immutable update guard");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_update", [])
            .expect("temporarily remove immutable update guard");
        forensic
            .execute(
                "UPDATE local_execution_attempt_outcomes SET outcome_id = ?1
                 WHERE operation_id = ?2 AND attempt_number = 0",
                params![
                    tampered_receipt.outcome_id.to_string(),
                    operation_id.to_string()
                ],
            )
            .expect("self-consistent local outcome tamper");
        forensic
            .execute(
                "UPDATE local_execution_r3_bridge_receipts
                 SET receipt_id = ?1, receipt_fingerprint = ?2, outcome_id = ?3
                 WHERE receipt_id = ?4",
                params![
                    tampered_id.to_string(),
                    tampered_fingerprint.as_slice(),
                    tampered_receipt.outcome_id.to_string(),
                    bridge_receipt_id(original_fingerprint).to_string(),
                ],
            )
            .expect("self-consistent tamper");
        forensic
            .execute_batch(LOCAL_EXECUTION_OUTCOMES_NO_UPDATE_TRIGGER)
            .expect("restore exact local outcome immutable guard");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable schema");
        assert_ne!(
            authoritative_outcome_id(
                operation_id,
                0,
                boundary.boundary_id,
                boundary.occurred_at_unix_ms,
                evidence_id,
                decision.evidence_fingerprint(),
                LocalExecutionOutcome::VerifiedApplied,
                decision.recorded_at_unix_ms(),
            ),
            tampered_receipt.outcome_id,
            "the tamper must be noncanonical, not merely a mismatched receipt"
        );
        assert!(matches!(
            store.commit_r3_change_batch(batch_id, 30),
            Err(Error::LocalMutationIncomplete)
        ));
        assert_eq!(
            store
                .active_change_batch()
                .expect("batch after rejection")
                .expect("still active")
                .committed_mutations,
            1
        );
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
        assert_eq!(count(&database_path, "local_execution_attempt_outcomes"), 1);
        assert_eq!(
            count(&database_path, "local_execution_r3_bridge_receipts"),
            1
        );
        forensic
            .execute("DROP TRIGGER local_execution_outcomes_no_update", [])
            .expect("temporarily remove local outcome immutable update guard");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_update", [])
            .expect("temporarily remove immutable update guard");
        forensic
            .execute(
                "UPDATE local_execution_attempt_outcomes SET outcome_id = ?1
                 WHERE operation_id = ?2 AND attempt_number = 0",
                params![
                    original_receipt.outcome_id.to_string(),
                    operation_id.to_string()
                ],
            )
            .expect("restore local outcome fixture only");
        forensic
            .execute(
                "UPDATE local_execution_r3_bridge_receipts
                 SET receipt_id = ?1, receipt_fingerprint = ?2, outcome_id = ?3
                 WHERE receipt_id = ?4",
                params![
                    bridge_receipt_id(original_fingerprint).to_string(),
                    original_fingerprint.as_slice(),
                    original_receipt.outcome_id.to_string(),
                    tampered_id.to_string(),
                ],
            )
            .expect("restore test fixture only");
        forensic
            .execute_batch(LOCAL_EXECUTION_OUTCOMES_NO_UPDATE_TRIGGER)
            .expect("restore exact local outcome immutable guard");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER)
            .expect("restore immutable schema");
        drop(forensic);
        let mut store = fixture.open();
        store
            .commit_r3_change_batch(batch_id, 30)
            .expect("cursor commit");
        assert_eq!(
            store
                .vault_state()
                .expect("state")
                .expect("bound")
                .durable_cursor,
            Some("cursor-2".into())
        );
    }

    #[test]
    fn r3_5_bridge_rejects_self_consistent_r3_intent_tamper() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, intent, evidence, decision, dependency, batch_id) =
            bridged_r3_5_batch(&fixture);
        let mut changed_intent = intent.clone();
        changed_intent.operation_marker = format!("{}-alternate", intent.operation_marker);
        changed_intent.intent_fingerprint = changed_intent.canonical_fingerprint();
        validate_mutation_intent(&changed_intent).expect("different valid canonical R3 intent");
        let original_receipt = bridge_receipt_for(
            &binding, &boundary, &intent, &evidence, &decision, dependency,
        );
        let original_id = bridge_receipt_id(bridge_receipt_fingerprint(&original_receipt));
        let mut tampered_receipt = original_receipt;
        tampered_receipt.r3_intent_fingerprint = changed_intent.intent_fingerprint.clone();
        let tampered_fingerprint = bridge_receipt_fingerprint(&tampered_receipt);
        let tampered_id = bridge_receipt_id(tampered_fingerprint);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_intents_no_update", [])
            .expect("remove intent guard");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_update", [])
            .expect("remove receipt guard");
        forensic
            .execute(
                "UPDATE mutation_intents
                    SET operation_marker = ?1, intent_fingerprint = ?2
                  WHERE operation_id = ?3",
                params![
                    changed_intent.operation_marker,
                    changed_intent.intent_fingerprint,
                    boundary.operation_id.to_string(),
                ],
            )
            .expect("self-consistent canonical R3 intent tamper");
        forensic
            .execute(
                "UPDATE local_execution_r3_bridge_receipts
                    SET receipt_id = ?1, receipt_fingerprint = ?2, r3_intent_fingerprint = ?3
                  WHERE receipt_id = ?4",
                params![
                    tampered_id.to_string(),
                    tampered_fingerprint.as_slice(),
                    tampered_receipt.r3_intent_fingerprint,
                    original_id.to_string(),
                ],
            )
            .expect("self-consistent receipt tamper");
        forensic
            .execute_batch(MUTATION_INTENTS_NO_UPDATE_TRIGGER)
            .expect("restore exact intent guard");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER)
            .expect("restore exact receipt guard");
        assert_ne!(
            binding.persistence_projection().intent_fingerprint,
            local_intent_fingerprint_from_r3_intent(&changed_intent.intent_fingerprint)
                .expect("local hash for alternate R3 intent"),
            "the local contract intentionally remains unchanged"
        );
        assert!(matches!(
            store.commit_r3_change_batch(batch_id, 30),
            Err(Error::LocalMutationIncomplete)
        ));
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
        assert_eq!(count(&database_path, "mutation_intents"), 1);
        assert_eq!(
            count(&database_path, "local_execution_r3_bridge_receipts"),
            1
        );
    }

    #[test]
    fn r3_5_bridge_final_state_fields_are_hash_bound_and_history_bound() {
        let fixture = Fixture::new();
        let (mut store, binding, boundary, intent, evidence, decision, dependency, batch_id) =
            bridged_r3_5_batch(&fixture);
        let original = bridge_receipt_for(
            &binding, &boundary, &intent, &evidence, &decision, dependency,
        );
        let original_fingerprint = bridge_receipt_fingerprint(&original);
        for alter in [
            |receipt: &mut BridgeReceiptFacts| receipt.r3_state_phase = "other".to_owned(),
            |receipt: &mut BridgeReceiptFacts| receipt.r3_state_disposition = "other".to_owned(),
            |receipt: &mut BridgeReceiptFacts| receipt.r3_last_evidence_id = Uuid::new_v4(),
            |receipt: &mut BridgeReceiptFacts| receipt.r3_state_version += 1,
            |receipt: &mut BridgeReceiptFacts| receipt.r3_event_state_version += 1,
        ] {
            let mut changed = original.clone();
            alter(&mut changed);
            assert_ne!(bridge_receipt_fingerprint(&changed), original_fingerprint);
        }
        let mut tampered = original;
        tampered.r3_state_version = 3;
        tampered.r3_event_state_version = 3;
        let tampered_fingerprint = bridge_receipt_fingerprint(&tampered);
        let original_id = bridge_receipt_id(original_fingerprint);
        let tampered_id = bridge_receipt_id(tampered_fingerprint);
        let database_path = store.database_path().to_owned();
        let forensic = Connection::open(&database_path).expect("forensic connection");
        forensic
            .execute("DROP TRIGGER mutation_events_no_update", [])
            .expect("remove event guard");
        forensic
            .execute("DROP TRIGGER local_execution_bridge_receipts_no_update", [])
            .expect("remove receipt guard");
        forensic
            .execute(
                "UPDATE mutation_state SET state_version = 3 WHERE operation_id = ?1",
                [boundary.operation_id.to_string()],
            )
            .expect("tamper R3 final state version");
        forensic
            .execute(
                "UPDATE mutation_events SET state_version = 3
                  WHERE operation_id = ?1 AND state_version = 2",
                [boundary.operation_id.to_string()],
            )
            .expect("tamper matching R3 final event version");
        forensic
            .execute(
                "UPDATE local_execution_r3_bridge_receipts
                    SET receipt_id = ?1, receipt_fingerprint = ?2,
                        r3_state_version = 3, r3_event_state_version = 3
                  WHERE receipt_id = ?3",
                params![
                    tampered_id.to_string(),
                    tampered_fingerprint.as_slice(),
                    original_id.to_string()
                ],
            )
            .expect("tamper self-consistent receipt final state");
        forensic
            .execute_batch(MUTATION_EVENTS_NO_UPDATE_TRIGGER)
            .expect("restore exact event guard");
        forensic
            .execute_batch(LOCAL_EXECUTION_BRIDGE_RECEIPTS_NO_UPDATE_TRIGGER)
            .expect("restore exact receipt guard");
        assert!(matches!(
            store.commit_r3_change_batch(batch_id, 30),
            Err(Error::LocalMutationIncomplete)
        ));
        drop(store);
        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
        assert_eq!(count(&database_path, "mutation_events"), 3);
        assert_eq!(
            count(&database_path, "local_execution_r3_bridge_receipts"),
            1
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn local_contract_cannot_retroactively_bless_started_or_committed_r3_work() {
        let fixture = Fixture::new();
        let mut store = ready_store(&fixture);
        let operation_id = Uuid::new_v4();
        let intent = local_publish_intent(operation_id);
        store
            .register_mutation_intent(&intent, None)
            .expect("R3 intent without local contract");
        store
            .claim_mutation(operation_id, 0, 12)
            .expect("R3 started");
        let evidence_id = Uuid::new_v4();
        let evidence = local_publish_evidence(evidence_id, &intent);
        store
            .record_mutation_outcome(
                operation_id,
                1,
                &evidence,
                &MutationOutcomeTransition::VerifiedApplied,
            )
            .expect("R3 completed");
        let binding = test_durable_execution_binding_for_r3_intent(
            operation_id,
            fixture.vault_id,
            &intent.intent_fingerprint,
        );
        assert!(matches!(
            store.register_local_execution_contract(&binding, 10),
            Err(Error::LocalExecutionCollision)
        ));
        assert_eq!(count(store.database_path(), "local_execution_contracts"), 0);

        // A complete public R3 batch can advance without a local contract;
        // it still cannot be blessed retroactively afterwards.
        let dependency = ChangeBatchDependency {
            operation_id,
            kind: ChangeBatchDependencyKind::Mutation,
        };
        let batch_id = Uuid::new_v4();
        store
            .begin_r3_change_batch(batch_id, "cursor-1", "cursor-2", &[dependency])
            .expect("typed R3 batch");
        store
            .commit_r3_change_dependency(batch_id, dependency, evidence_id)
            .expect("public dependency commit");
        store
            .commit_r3_change_batch(batch_id, 30)
            .expect("public cursor advance");
        drop(store);
        let mut reopened = fixture.open();
        assert!(matches!(
            reopened.register_local_execution_contract(&binding, 10),
            Err(Error::LocalExecutionCollision)
        ));
        assert_eq!(
            count(reopened.database_path(), "local_execution_contracts"),
            0
        );

        // The same rule also holds while a completed public dependency is
        // still inside an active batch; no restart can turn it into R3.5
        // authority.
        let active_operation = Uuid::new_v4();
        let active_intent = local_publish_intent(active_operation);
        reopened
            .register_mutation_intent(&active_intent, None)
            .expect("second R3 intent");
        reopened
            .claim_mutation(active_operation, 0, 40)
            .expect("second R3 start");
        let active_evidence_id = Uuid::new_v4();
        let mut active_evidence = local_publish_evidence(active_evidence_id, &active_intent);
        active_evidence.captured_at_unix_ms = 40;
        active_evidence.evidence_fingerprint = active_evidence.canonical_fingerprint();
        reopened
            .record_mutation_outcome(
                active_operation,
                1,
                &active_evidence,
                &MutationOutcomeTransition::VerifiedApplied,
            )
            .expect("second R3 completion");
        let active_dependency = ChangeBatchDependency {
            operation_id: active_operation,
            kind: ChangeBatchDependencyKind::Mutation,
        };
        let active_batch = Uuid::new_v4();
        reopened
            .begin_r3_change_batch(active_batch, "cursor-2", "cursor-3", &[active_dependency])
            .expect("active typed batch");
        reopened
            .commit_r3_change_dependency(active_batch, active_dependency, active_evidence_id)
            .expect("active public dependency commit");
        // Restart in the narrow interval after the public dependency commits
        // but before its cursor finalizes.  This remains a pure R3 batch;
        // reopening cannot manufacture R3.5 authority retroactively.
        drop(reopened);
        let mut reopened = fixture.open();
        let active_binding = test_durable_execution_binding_for_r3_intent(
            active_operation,
            fixture.vault_id,
            &active_intent.intent_fingerprint,
        );
        assert!(matches!(
            reopened.register_local_execution_contract(&active_binding, 40),
            Err(Error::LocalExecutionCollision)
        ));
        reopened
            .commit_r3_change_batch(active_batch, 50)
            .expect("pure R3 cursor finalization after restart");
        assert_eq!(
            count(reopened.database_path(), "local_execution_contracts"),
            0,
            "no retroactive R3.5 contract can appear"
        );
        assert_eq!(
            reopened
                .vault_state()
                .expect("state after pure R3 cursor")
                .expect("bound state")
                .durable_cursor,
            Some("cursor-3".into())
        );
    }

    #[test]
    fn authoritative_finalization_resumes_witness_before_ledger_without_duplication() {
        let (_fixture, mut store, binding, boundary) = prepared_journal_attempt();
        store
            .publish_local_execution_pre_side_effect_witness(&binding, &boundary, 11)
            .expect("pre witness");
        let decision = classify_authoritative_final_outcome(
            &binding,
            &boundary,
            test_authoritative_evidence(
                &binding,
                &boundary,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
            ),
        )
        .expect("test evidence");
        let different_boundary = LocalExecutionAttemptBoundary {
            boundary_id: Uuid::new_v4(),
            ..boundary
        };
        let decision_for_different_boundary = classify_authoritative_final_outcome(
            &binding,
            &different_boundary,
            test_authoritative_evidence(
                &binding,
                &different_boundary,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
            ),
        )
        .expect("different-boundary classification");
        assert!(matches!(
            store.finalize_authoritative_local_execution_outcome(
                &binding,
                &boundary,
                &decision_for_different_boundary,
            ),
            Err(Error::LocalExecutionJournalMismatch)
        ));
        let pending = LocalExecutionAttemptOutcome {
            operation_id: boundary.operation_id,
            attempt_number: boundary.attempt_number,
            outcome_id: decision.outcome_id(),
            evidence_id: decision.evidence_id(),
            outcome: decision.outcome(),
            evidence_fingerprint: decision.evidence_fingerprint(),
            occurred_at_unix_ms: decision.recorded_at_unix_ms(),
        };
        store
            .publish_local_execution_outcome_witness_with_r3_evidence(
                &binding,
                &boundary,
                &pending,
                Some(decision.r3_mutation_evidence_fingerprint()),
            )
            .expect("outcome witness");
        assert!(matches!(
            store
                .inspect_local_execution_recovery(&binding, boundary.attempt_number)
                .expect("recovery"),
            LocalExecutionRecoveryObservation::OutcomeWitnessPendingLedger { .. }
        ));
        assert_eq!(
            store
                .finalize_authoritative_local_execution_outcome(&binding, &boundary, &decision)
                .expect("resume ledger"),
            LocalExecutionRegistrationOutcome::Registered
        );
        assert_eq!(
            store
                .finalize_authoritative_local_execution_outcome(&binding, &boundary, &decision)
                .expect("idempotent finalization"),
            LocalExecutionRegistrationOutcome::AlreadyPresent
        );
        assert!(matches!(
            store
                .inspect_local_execution_recovery(&binding, boundary.attempt_number)
                .expect("recovery"),
            LocalExecutionRecoveryObservation::OutcomeWitnessAndLedgerMatch { .. }
        ));
    }

    #[test]
    fn echo_hints_are_inventory_only_and_make_no_durable_writes() {
        let (_fixture, mut store, binding, boundary) = prepared_journal_attempt();
        let hint = LocalExecutionEchoHint::new(
            boundary.operation_id,
            *binding.fingerprint().as_bytes(),
            LocalExecutionEchoSource::AndroidSaf,
        );
        let before_boundaries = count(store.database_path(), "local_execution_attempt_boundaries");
        let before_outcomes = count(store.database_path(), "local_execution_attempt_outcomes");
        assert_eq!(
            handle_local_execution_echo_hint(&store, &binding, boundary.attempt_number, hint)
                .expect("first hint"),
            EchoHintDisposition::InventoryRequired
        );
        assert_eq!(
            handle_local_execution_echo_hint(&store, &binding, boundary.attempt_number, hint)
                .expect("repeated hint"),
            EchoHintDisposition::InventoryRequired
        );
        assert_eq!(
            count(store.database_path(), "local_execution_attempt_boundaries"),
            before_boundaries
        );
        assert_eq!(
            count(store.database_path(), "local_execution_attempt_outcomes"),
            before_outcomes
        );
        store
            .publish_local_execution_pre_side_effect_witness(&binding, &boundary, 11)
            .expect("pre witness");
        let decision = classify_authoritative_final_outcome(
            &binding,
            &boundary,
            test_authoritative_evidence(
                &binding,
                &boundary,
                PlatformCallFact::Returned,
                LocalExecutionOutcome::VerifiedApplied,
            ),
        )
        .expect("test evidence");
        store
            .finalize_authoritative_local_execution_outcome(&binding, &boundary, &decision)
            .expect("finalize");
        assert_eq!(
            handle_local_execution_echo_hint(&store, &binding, boundary.attempt_number, hint)
                .expect("observed hint"),
            EchoHintDisposition::DurableClaimPresentInventoryRequired
        );
    }

    #[test]
    fn local_execution_registration_is_atomic_at_every_contract_row_boundary() {
        for (name, table) in [
            ("fault_local_contract", "local_execution_contracts"),
            ("fault_local_identity", "local_execution_identity_evidence"),
            ("fault_local_member", "local_execution_collision_members"),
        ] {
            let fixture = Fixture::new();
            let mut store = fixture.open();
            let operation_id = Uuid::new_v4();
            let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
            install_abort_trigger(store.database_path(), name, table);
            assert!(store
                .register_local_execution_contract(&binding, 10)
                .is_err());
            for table in [
                "local_execution_contracts",
                "local_execution_identity_evidence",
                "local_execution_collision_members",
                "local_execution_contract_completions",
            ] {
                assert_eq!(count(store.database_path(), table), 0, "{name}: {table}");
            }
        }
    }

    #[test]
    fn local_execution_first_issuance_attestations_survive_rotated_reissue_and_reopen() {
        let fixture = Fixture::new();
        let operation_id = Uuid::new_v4();
        let first = test_durable_execution_binding_with_attestation_offset(
            operation_id,
            fixture.vault_id,
            0,
        );
        let rotated = test_durable_execution_binding_with_attestation_offset(
            operation_id,
            fixture.vault_id,
            19,
        );
        assert_eq!(first.fingerprint(), rotated.fingerprint());
        let mut store = fixture.open();
        assert_eq!(
            store
                .register_local_execution_contract(&first, 10)
                .expect("first issuance"),
            LocalExecutionRegistrationOutcome::Registered
        );
        let contract_before = store
            .local_execution_contract(operation_id)
            .expect("contract read")
            .expect("stored contract");
        let database_path = store.database_path().to_owned();
        let attestation_rows = |path: &Path| {
            let connection = Connection::open(path).expect("read forensic attestation rows");
            let mut statement = connection
                .prepare(
                    "SELECT role, attestation FROM local_execution_identity_evidence
                       WHERE operation_id = ?1
                     UNION ALL
                     SELECT printf('member:%08d', ordinal), attestation
                       FROM local_execution_collision_members WHERE operation_id = ?1
                     ORDER BY 1",
                )
                .expect("attestation query");
            statement
                .query_map([operation_id.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .expect("attestation rows")
                .collect::<std::result::Result<Vec<_>, _>>()
                .expect("collect attestations")
        };
        let first_attestations = attestation_rows(&database_path);
        assert_eq!(first_attestations.len(), 7, "six roles plus one member");
        assert_eq!(
            store
                .register_local_execution_contract(&rotated, 10)
                .expect("rotated exact stable reissue"),
            LocalExecutionRegistrationOutcome::AlreadyPresent
        );
        assert_eq!(attestation_rows(&database_path), first_attestations);
        assert_eq!(
            store
                .local_execution_contract(operation_id)
                .expect("contract after reissue"),
            Some(contract_before.clone())
        );
        drop(store);
        let reopened = fixture.open();
        assert_eq!(attestation_rows(&database_path), first_attestations);
        assert_eq!(
            reopened
                .local_execution_contract(operation_id)
                .expect("contract after reopen"),
            Some(contract_before)
        );
    }

    fn populated_local_contract(fixture: &Fixture) -> (SyncStore, LocalExecutionAttemptBoundary) {
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        let mut store = fixture.open();
        store
            .register_local_execution_contract(&binding, 10)
            .expect("complete contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("complete boundary");
        let evidence_id = Uuid::new_v4();
        let outcome = LocalExecutionAttemptOutcome {
            operation_id,
            attempt_number: 0,
            outcome_id: authoritative_outcome_id(
                operation_id,
                0,
                boundary.boundary_id,
                boundary.occurred_at_unix_ms,
                evidence_id,
                [9; 32],
                LocalExecutionOutcome::VerifiedApplied,
                12,
            ),
            evidence_id,
            outcome: LocalExecutionOutcome::VerifiedApplied,
            evidence_fingerprint: [9; 32],
            occurred_at_unix_ms: 12,
        };
        store
            .append_local_execution_attempt_outcome(&outcome)
            .expect("complete outcome");
        (store, boundary)
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn populated_local_contract_semantic_reopen_tamper_matrix() {
        enum Tamper {
            IdentityProviderPreimage,
            IdentityStableFingerprint,
            CollisionMemberCanonicalKey,
            ContractVault,
            NilBoundaryIdentifier,
            BoundaryAttemptOutOfRange,
        }
        for tamper in [
            Tamper::IdentityProviderPreimage,
            Tamper::IdentityStableFingerprint,
            Tamper::CollisionMemberCanonicalKey,
            Tamper::ContractVault,
            Tamper::NilBoundaryIdentifier,
            Tamper::BoundaryAttemptOutOfRange,
        ] {
            let fixture = Fixture::new();
            let (store, boundary) = populated_local_contract(&fixture);
            let database_path = store.database_path().to_owned();
            drop(store);
            let forensic = Connection::open(&database_path).expect("forensic connection");
            let operation = boundary.operation_id.to_string();
            let name = match tamper {
                Tamper::IdentityProviderPreimage => {
                    forensic
                        .execute("DROP TRIGGER local_execution_identities_no_update", [])
                        .expect("remove identity guard");
                    forensic
                        .execute(
                            "UPDATE local_execution_identity_evidence SET provider_id = X'01'
                              WHERE operation_id = ?1 AND role = 'vault_root'",
                            [&operation],
                        )
                        .expect("alter provider preimage only");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_IDENTITIES_NO_UPDATE_TRIGGER)
                        .expect("restore exact identity guard");
                    "identity_provider_preimage"
                }
                Tamper::IdentityStableFingerprint => {
                    forensic
                        .execute("DROP TRIGGER local_execution_identities_no_update", [])
                        .expect("remove identity guard");
                    forensic
                        .execute(
                            "UPDATE local_execution_identity_evidence
                                SET stable_identity_fingerprint = zeroblob(32)
                              WHERE operation_id = ?1 AND role = 'source_parent'",
                            [&operation],
                        )
                        .expect("alter CHECK-valid stable fingerprint");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_IDENTITIES_NO_UPDATE_TRIGGER)
                        .expect("restore exact identity guard");
                    "identity_stable_fingerprint"
                }
                Tamper::CollisionMemberCanonicalKey => {
                    forensic
                        .execute("DROP TRIGGER local_execution_members_no_update", [])
                        .expect("remove member guard");
                    forensic
                        .execute(
                            "UPDATE local_execution_collision_members SET collision_key = 'not-canonical'
                              WHERE operation_id = ?1 AND ordinal = 0",
                            [&operation],
                        )
                        .expect("alter member canonical key");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_MEMBERS_NO_UPDATE_TRIGGER)
                        .expect("restore exact member guard");
                    "collision_member_canonical_key"
                }
                Tamper::ContractVault => {
                    forensic
                        .execute("DROP TRIGGER local_execution_contracts_no_update", [])
                        .expect("remove contract guard");
                    forensic
                        .execute(
                            "UPDATE local_execution_contracts SET vault_id = ?1 WHERE operation_id = ?2",
                            params![Uuid::new_v4().to_string(), operation],
                        )
                        .expect("alter expected vault identifier");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_CONTRACTS_NO_UPDATE_TRIGGER)
                        .expect("restore exact contract guard");
                    "contract_expected_vault"
                }
                Tamper::NilBoundaryIdentifier => {
                    forensic
                        .execute("DROP TRIGGER local_execution_boundaries_no_update", [])
                        .expect("remove boundary guard");
                    forensic
                        .execute(
                            "UPDATE local_execution_attempt_boundaries SET boundary_id = ?1
                              WHERE operation_id = ?2 AND attempt_number = 0",
                            params![Uuid::nil().to_string(), operation],
                        )
                        .expect("alter CHECK-valid nil boundary identifier");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_BOUNDARIES_NO_UPDATE_TRIGGER)
                        .expect("restore exact boundary guard");
                    "nil_boundary_identifier"
                }
                Tamper::BoundaryAttemptOutOfRange => {
                    forensic
                        .execute("DROP TRIGGER local_execution_boundaries_no_update", [])
                        .expect("remove boundary guard");
                    forensic
                        .execute("DROP TRIGGER local_execution_outcomes_no_update", [])
                        .expect("remove outcome guard");
                    forensic
                        .pragma_update(None, "foreign_keys", false)
                        .expect("temporarily defer forensic foreign-key enforcement");
                    forensic
                        .execute(
                            "UPDATE local_execution_attempt_boundaries SET attempt_number = 4294967296
                              WHERE operation_id = ?1 AND attempt_number = 0",
                            [&operation],
                        )
                        .expect("alter CHECK-valid boundary attempt range");
                    forensic
                        .execute(
                            "UPDATE local_execution_attempt_outcomes SET attempt_number = 4294967296
                              WHERE operation_id = ?1 AND attempt_number = 0",
                            [&operation],
                        )
                        .expect("keep outcome relation self-consistent");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_BOUNDARIES_NO_UPDATE_TRIGGER)
                        .expect("restore exact boundary guard");
                    forensic
                        .execute_batch(LOCAL_EXECUTION_OUTCOMES_NO_UPDATE_TRIGGER)
                        .expect("restore exact outcome guard");
                    forensic
                        .pragma_update(None, "foreign_keys", true)
                        .expect("restore forensic foreign-key enforcement");
                    "boundary_attempt_out_of_range"
                }
            };
            assert!(matches!(
                SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
                Err(Error::InvalidSchema)
            ));
            assert_eq!(
                count(&database_path, "local_execution_contracts"),
                1,
                "{name}"
            );
            assert_eq!(
                count(&database_path, "local_execution_identity_evidence"),
                6,
                "{name}"
            );
            assert_eq!(
                count(&database_path, "local_execution_collision_members"),
                1,
                "{name}"
            );
            assert_eq!(
                count(&database_path, "local_execution_attempt_boundaries"),
                1,
                "{name}"
            );
            assert_eq!(
                count(&database_path, "local_execution_attempt_outcomes"),
                1,
                "{name}"
            );
        }
    }

    #[test]
    fn local_execution_attempt_and_outcome_faults_roll_back_without_fabrication() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        assert_eq!(
            store
                .register_local_execution_contract(&binding, 10)
                .expect("contract"),
            LocalExecutionRegistrationOutcome::Registered
        );
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        install_abort_trigger(
            store.database_path(),
            "fault_local_boundary",
            "local_execution_attempt_boundaries",
        );
        let boundary_fault = store.append_local_execution_attempt_boundary(&boundary);
        assert!(matches!(
            boundary_fault,
            Err(Error::Database(error)) if error.to_string().contains("injected fault")
        ));
        assert_eq!(
            count(store.database_path(), "local_execution_attempt_boundaries"),
            0
        );
        Connection::open(store.database_path())
            .expect("fault connection")
            .execute("DROP TRIGGER fault_local_boundary", [])
            .expect("drop fault trigger");
        assert_eq!(
            store
                .append_local_execution_attempt_boundary(&boundary)
                .expect("boundary"),
            LocalExecutionRegistrationOutcome::Registered
        );
        let evidence_id = Uuid::new_v4();
        let outcome = LocalExecutionAttemptOutcome {
            operation_id,
            attempt_number: 0,
            outcome_id: authoritative_outcome_id(
                operation_id,
                0,
                boundary.boundary_id,
                boundary.occurred_at_unix_ms,
                evidence_id,
                [9; 32],
                LocalExecutionOutcome::WriteOutcomeUnknown,
                12,
            ),
            evidence_id,
            outcome: LocalExecutionOutcome::WriteOutcomeUnknown,
            evidence_fingerprint: [9; 32],
            occurred_at_unix_ms: 12,
        };
        install_abort_trigger(
            store.database_path(),
            "fault_local_outcome",
            "local_execution_attempt_outcomes",
        );
        let outcome_fault = store.append_local_execution_attempt_outcome(&outcome);
        assert!(matches!(
            outcome_fault,
            Err(Error::Database(error)) if error.to_string().contains("injected fault")
        ));
        assert_eq!(
            count(store.database_path(), "local_execution_attempt_outcomes"),
            0
        );
        Connection::open(store.database_path())
            .expect("fault connection")
            .execute("DROP TRIGGER fault_local_outcome", [])
            .expect("drop outcome fault trigger");
        assert_eq!(
            store
                .append_local_execution_attempt_outcome(&outcome)
                .expect("canonical outcome inserts after trigger removal"),
            LocalExecutionRegistrationOutcome::Registered
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn local_execution_records_are_exactly_idempotent_and_reject_fact_drift() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        assert_eq!(
            store
                .register_local_execution_contract(&binding, 10)
                .expect("contract"),
            LocalExecutionRegistrationOutcome::Registered
        );
        assert_eq!(
            store
                .register_local_execution_contract(&binding, 10)
                .expect("repeat contract"),
            LocalExecutionRegistrationOutcome::AlreadyPresent
        );
        assert!(matches!(
            store.register_local_execution_contract(&binding, 11),
            Err(Error::LocalExecutionCollision)
        ));
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 12,
        };
        assert_eq!(
            store
                .append_local_execution_attempt_boundary(&boundary)
                .expect("boundary"),
            LocalExecutionRegistrationOutcome::Registered
        );
        assert_eq!(
            store
                .append_local_execution_attempt_boundary(&boundary)
                .expect("repeat boundary"),
            LocalExecutionRegistrationOutcome::AlreadyPresent
        );
        let outcome = LocalExecutionAttemptOutcome {
            operation_id,
            attempt_number: 0,
            outcome_id: Uuid::nil(),
            evidence_id: Uuid::new_v4(),
            outcome: LocalExecutionOutcome::WriteOutcomeUnknown,
            evidence_fingerprint: [9; 32],
            occurred_at_unix_ms: 13,
        };
        let outcome = LocalExecutionAttemptOutcome {
            outcome_id: authoritative_outcome_id(
                operation_id,
                0,
                boundary.boundary_id,
                boundary.occurred_at_unix_ms,
                outcome.evidence_id,
                outcome.evidence_fingerprint,
                outcome.outcome,
                outcome.occurred_at_unix_ms,
            ),
            ..outcome
        };
        assert_eq!(
            store
                .append_local_execution_attempt_outcome(&outcome)
                .expect("outcome"),
            LocalExecutionRegistrationOutcome::Registered
        );
        assert_eq!(
            store
                .append_local_execution_attempt_outcome(&outcome)
                .expect("repeat outcome"),
            LocalExecutionRegistrationOutcome::AlreadyPresent
        );
        let mut drifted_outcome = outcome.clone();
        drifted_outcome.evidence_fingerprint = [8; 32];
        assert!(matches!(
            store.append_local_execution_attempt_outcome(&drifted_outcome),
            Err(Error::InvalidLocalExecutionEvidence)
        ));

        let reconcile_boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 1,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 14,
        };
        store
            .append_local_execution_attempt_boundary(&reconcile_boundary)
            .expect("reconcile boundary");
        let reconcile_evidence_id = Uuid::new_v4();
        let reconcile_outcome = LocalExecutionAttemptOutcome {
            operation_id,
            attempt_number: 1,
            outcome_id: authoritative_outcome_id(
                operation_id,
                1,
                reconcile_boundary.boundary_id,
                reconcile_boundary.occurred_at_unix_ms,
                reconcile_evidence_id,
                [7; 32],
                LocalExecutionOutcome::NeedsReconcile,
                15,
            ),
            evidence_id: reconcile_evidence_id,
            outcome: LocalExecutionOutcome::NeedsReconcile,
            evidence_fingerprint: [7; 32],
            occurred_at_unix_ms: 15,
        };
        store
            .append_local_execution_attempt_outcome(&reconcile_outcome)
            .expect("needs reconcile outcome");
        let persisted = store
            .local_execution_attempt_outcome(operation_id, 1)
            .expect("read outcome")
            .expect("persisted outcome");
        assert_eq!(persisted.evidence_id, reconcile_outcome.evidence_id);
        assert!(persisted.non_retryable);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn deterministic_local_outcome_ids_publish_append_and_restart_for_every_variant() {
        for (index, outcome) in [
            LocalExecutionOutcome::VerifiedApplied,
            LocalExecutionOutcome::VerifiedNotApplied,
            LocalExecutionOutcome::WriteOutcomeUnknown,
            LocalExecutionOutcome::NeedsReconcile,
        ]
        .into_iter()
        .enumerate()
        {
            let fixture = Fixture::new();
            let operation_id = Uuid::new_v4();
            let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
            let boundary = LocalExecutionAttemptBoundary {
                operation_id,
                attempt_number: 0,
                boundary_id: Uuid::new_v4(),
                contract_fingerprint: binding.fingerprint(),
                occurred_at_unix_ms: 10,
            };
            let evidence_id = Uuid::new_v4();
            let evidence_fingerprint = [u8::try_from(index + 1).expect("small index"); 32];
            let local_outcome = LocalExecutionAttemptOutcome {
                operation_id,
                attempt_number: 0,
                outcome_id: authoritative_outcome_id(
                    operation_id,
                    0,
                    boundary.boundary_id,
                    boundary.occurred_at_unix_ms,
                    evidence_id,
                    evidence_fingerprint,
                    outcome,
                    11,
                ),
                evidence_id,
                outcome,
                evidence_fingerprint,
                occurred_at_unix_ms: 11,
            };
            let mut store = fixture.open();
            store
                .register_local_execution_contract(&binding, 9)
                .expect("contract");
            store
                .append_local_execution_attempt_boundary(&boundary)
                .expect("boundary");
            store
                .publish_local_execution_pre_side_effect_witness(&binding, &boundary, 10)
                .expect("pre witness");
            assert_eq!(
                store
                    .publish_local_execution_outcome_witness(&binding, &boundary, &local_outcome)
                    .expect("canonical outcome witness"),
                LocalExecutionWitnessPublicationOutcome::Published
            );
            assert_eq!(
                store
                    .append_local_execution_attempt_outcome(&local_outcome)
                    .expect("canonical outcome ledger"),
                LocalExecutionRegistrationOutcome::Registered
            );
            drop(store);
            let reopened = fixture.open();
            assert_eq!(
                reopened
                    .local_execution_attempt_outcome(operation_id, 0)
                    .expect("read after restart")
                    .expect("persisted after restart")
                    .outcome_id,
                local_outcome.outcome_id
            );
            drop(reopened);

            let bad_fixture = Fixture::new();
            let bad_operation = Uuid::new_v4();
            let bad_binding = test_durable_execution_binding(bad_operation, bad_fixture.vault_id);
            let bad_boundary = LocalExecutionAttemptBoundary {
                operation_id: bad_operation,
                attempt_number: 0,
                boundary_id: Uuid::new_v4(),
                contract_fingerprint: bad_binding.fingerprint(),
                occurred_at_unix_ms: 10,
            };
            let mut bad = local_outcome.clone();
            bad.operation_id = bad_operation;
            bad.outcome_id = Uuid::new_v4();
            let mut bad_store = bad_fixture.open();
            bad_store
                .register_local_execution_contract(&bad_binding, 9)
                .expect("bad contract");
            bad_store
                .append_local_execution_attempt_boundary(&bad_boundary)
                .expect("bad boundary");
            bad_store
                .publish_local_execution_pre_side_effect_witness(&bad_binding, &bad_boundary, 10)
                .expect("bad pre witness");
            let outcomes_before = count(
                bad_store.database_path(),
                "local_execution_attempt_outcomes",
            );
            let journal_before = fs::read_dir(bad_fixture.journal_directory())
                .expect("journal dir")
                .count();
            assert!(matches!(
                bad_store.publish_local_execution_outcome_witness(
                    &bad_binding,
                    &bad_boundary,
                    &bad
                ),
                Err(Error::InvalidLocalExecutionEvidence)
            ));
            assert!(matches!(
                bad_store.append_local_execution_attempt_outcome(&bad),
                Err(Error::InvalidLocalExecutionEvidence)
            ));
            assert_eq!(
                count(
                    bad_store.database_path(),
                    "local_execution_attempt_outcomes"
                ),
                outcomes_before
            );
            assert_eq!(
                fs::read_dir(bad_fixture.journal_directory())
                    .expect("journal dir")
                    .count(),
                journal_before
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn execution_journal_preserves_all_attempt_crash_boundaries_without_authority() {
        let fixture = Fixture::new();
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        let evidence_id = Uuid::new_v4();
        let outcome = LocalExecutionAttemptOutcome {
            operation_id,
            attempt_number: 0,
            outcome_id: authoritative_outcome_id(
                operation_id,
                0,
                boundary.boundary_id,
                boundary.occurred_at_unix_ms,
                evidence_id,
                [7; 32],
                LocalExecutionOutcome::VerifiedApplied,
                13,
            ),
            evidence_id,
            outcome: LocalExecutionOutcome::VerifiedApplied,
            evidence_fingerprint: [7; 32],
            occurred_at_unix_ms: 13,
        };

        let mut store = fixture.open();
        store
            .register_local_execution_contract(&binding, 10)
            .expect("contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("boundary");
        assert_eq!(
            store
                .inspect_local_execution_recovery(&binding, 0)
                .expect("before witness"),
            LocalExecutionRecoveryObservation::BoundaryWithoutWitness
        );
        drop(store);
        let store = fixture.open();
        let binding_after_boundary = test_durable_execution_binding(operation_id, fixture.vault_id);
        assert_eq!(
            store
                .publish_local_execution_pre_side_effect_witness(
                    &binding_after_boundary,
                    &boundary,
                    boundary.occurred_at_unix_ms,
                )
                .expect("pre witness"),
            LocalExecutionWitnessPublicationOutcome::Published
        );
        assert_eq!(
            store
                .publish_local_execution_pre_side_effect_witness(
                    &binding_after_boundary,
                    &boundary,
                    boundary.occurred_at_unix_ms,
                )
                .expect("idempotent pre witness"),
            LocalExecutionWitnessPublicationOutcome::AlreadyPublished
        );
        assert_eq!(
            store
                .inspect_local_execution_recovery(&binding_after_boundary, 0)
                .expect("platform-call window"),
            LocalExecutionRecoveryObservation::PreSideEffectWitnessOnly
        );
        drop(store);
        let mut store = fixture.open();
        let binding_after_pre = test_durable_execution_binding(operation_id, fixture.vault_id);
        assert_eq!(
            store
                .inspect_local_execution_recovery(&binding_after_pre, 0)
                .expect("restart during platform-call window"),
            LocalExecutionRecoveryObservation::PreSideEffectWitnessOnly
        );
        let mut unpersistable_outcome = outcome.clone();
        unpersistable_outcome.occurred_at_unix_ms = (i64::MAX as u64) + 1;
        assert!(matches!(
            store.publish_local_execution_outcome_witness(
                &binding_after_pre,
                &boundary,
                &unpersistable_outcome,
            ),
            Err(Error::InvalidTimestamp)
        ));
        assert!(store
            .execution_journal
            .read_outcome(operation_id, 0)
            .expect("read journal after rejected timestamp")
            .is_none());
        assert_eq!(
            store
                .publish_local_execution_outcome_witness(&binding_after_pre, &boundary, &outcome,)
                .expect("outcome witness"),
            LocalExecutionWitnessPublicationOutcome::Published
        );
        let published_outcome = store
            .execution_journal
            .read_outcome(operation_id, 0)
            .expect("read outcome witness")
            .expect("published outcome witness");
        assert_eq!(
            published_outcome.pre.created_at_unix_ms,
            boundary.occurred_at_unix_ms
        );
        assert_eq!(published_outcome.created_at_unix_ms, 13);
        assert_eq!(
            store
                .inspect_local_execution_recovery(&binding_after_pre, 0)
                .expect("before ledger outcome"),
            LocalExecutionRecoveryObservation::OutcomeWitnessPendingLedger {
                claim: UntrustedLocalExecutionOutcomeClaim::VerifiedApplied,
            }
        );
        install_abort_trigger(
            store.database_path(),
            "fault_outcome_after_journal",
            "local_execution_attempt_outcomes",
        );
        assert!(store
            .append_local_execution_attempt_outcome(&outcome)
            .is_err());
        assert_eq!(
            count(store.database_path(), "local_execution_attempt_outcomes"),
            0
        );
        Connection::open(store.database_path())
            .expect("fault connection")
            .execute("DROP TRIGGER fault_outcome_after_journal", [])
            .expect("drop outcome fault trigger");
        drop(store);

        let mut store = fixture.open();
        let binding_after_outcome = test_durable_execution_binding(operation_id, fixture.vault_id);
        assert_eq!(
            store
                .inspect_local_execution_recovery(&binding_after_outcome, 0)
                .expect("restart before ledger commit"),
            LocalExecutionRecoveryObservation::OutcomeWitnessPendingLedger {
                claim: UntrustedLocalExecutionOutcomeClaim::VerifiedApplied,
            }
        );
        store
            .append_local_execution_attempt_outcome(&outcome)
            .expect("ledger outcome after restart");
        assert_eq!(
            store
                .inspect_local_execution_recovery(&binding_after_outcome, 0)
                .expect("matching ledger outcome"),
            LocalExecutionRecoveryObservation::OutcomeWitnessAndLedgerMatch {
                claim: UntrustedLocalExecutionOutcomeClaim::VerifiedApplied,
            }
        );
        drop(store);

        let mut reopened = fixture.open();
        let freshly_reissued_binding =
            test_durable_execution_binding(operation_id, fixture.vault_id);
        assert_eq!(
            reopened
                .inspect_local_execution_recovery(&freshly_reissued_binding, 0)
                .expect("restart observation"),
            LocalExecutionRecoveryObservation::OutcomeWitnessAndLedgerMatch {
                claim: UntrustedLocalExecutionOutcomeClaim::VerifiedApplied,
            }
        );
        assert_eq!(
            reopened
                .append_local_execution_attempt_boundary(&boundary)
                .expect("no duplicate boundary"),
            LocalExecutionRegistrationOutcome::AlreadyPresent
        );
        assert_eq!(
            reopened
                .append_local_execution_attempt_outcome(&outcome)
                .expect("no duplicate outcome"),
            LocalExecutionRegistrationOutcome::AlreadyPresent
        );
    }

    #[test]
    fn outcome_witness_cannot_be_fabricated_after_the_ledger_outcome() {
        let fixture = Fixture::new();
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        let evidence_id = Uuid::new_v4();
        let outcome = LocalExecutionAttemptOutcome {
            operation_id,
            attempt_number: 0,
            outcome_id: authoritative_outcome_id(
                operation_id,
                0,
                boundary.boundary_id,
                boundary.occurred_at_unix_ms,
                evidence_id,
                [7; 32],
                LocalExecutionOutcome::VerifiedApplied,
                13,
            ),
            evidence_id,
            outcome: LocalExecutionOutcome::VerifiedApplied,
            evidence_fingerprint: [7; 32],
            occurred_at_unix_ms: 13,
        };
        let mut store = fixture.open();
        store
            .register_local_execution_contract(&binding, 10)
            .expect("contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("boundary");
        store
            .publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            )
            .expect("pre witness");
        store
            .append_local_execution_attempt_outcome(&outcome)
            .expect("ledger outcome injected before journal outcome");

        assert!(matches!(
            store.inspect_local_execution_recovery(&binding, 0),
            Err(Error::LocalExecutionJournalMismatch)
        ));
        assert!(matches!(
            store.publish_local_execution_outcome_witness(&binding, &boundary, &outcome),
            Err(Error::LocalExecutionJournalMismatch)
        ));
        assert!(store
            .execution_journal
            .read_outcome(operation_id, 0)
            .expect("read journal")
            .is_none());
    }

    #[test]
    fn published_witness_survives_directory_sync_failure() {
        let fixture = Fixture::new();
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        let mut store = fixture.open();
        store
            .register_local_execution_contract(&binding, 10)
            .expect("contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("boundary");
        let journal = fixture.journal_directory();
        let temporary_before_fault = sync_journal_temporary_count(&journal);
        store.execution_journal.fail_next_directory_sync_for_test();
        assert!(matches!(
            store.publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            ),
            Err(Error::LocalExecutionJournalPublishedButNotSynced(_))
        ));
        let temporary_after_initial_fault = sync_journal_temporary_count(&journal);
        assert_eq!(temporary_after_initial_fault, temporary_before_fault);
        assert_eq!(
            store
                .inspect_local_execution_recovery(&binding, 0)
                .expect("published witness remains readable"),
            LocalExecutionRecoveryObservation::PreSideEffectWitnessOnly
        );
        // Exact-final retry creates no temp, but it must still execute the
        // directory durability repair.  Re-arm the injected fsync fault to
        // prove the retry consumes it rather than returning early.
        store.execution_journal.fail_next_directory_sync_for_test();
        assert!(matches!(
            store.publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            ),
            Err(Error::LocalExecutionJournalPublishedButNotSynced(_))
        ));
        assert_eq!(
            sync_journal_temporary_count(&journal),
            temporary_after_initial_fault,
            "exact-final retry must not create a journal temporary before resync"
        );
        assert_eq!(
            store
                .publish_local_execution_pre_side_effect_witness(
                    &binding,
                    &boundary,
                    boundary.occurred_at_unix_ms,
                )
                .expect("identical retry resyncs"),
            LocalExecutionWitnessPublicationOutcome::AlreadyPublished
        );
        assert_eq!(
            sync_journal_temporary_count(&journal),
            temporary_after_initial_fault,
            "successful exact retry must not create a journal temporary"
        );
    }

    #[test]
    fn deterministic_temp_crash_and_reuse_must_cross_file_sync_barrier() {
        let (fixture, store, binding, boundary) = prepared_journal_attempt();
        let journal = fixture.journal_directory();
        let temporary = journal.join(format!(
            ".sync-execution-witness-{}-{}.pre.tmp",
            boundary.operation_id, boundary.attempt_number
        ));
        let expected = store
            .exact_execution_witness(&binding, &boundary, boundary.occurred_at_unix_ms)
            .expect("exact expected witness");
        let bytes = crate::sync_journal::canonical_pre_bytes_for_test(&expected);
        fs::write(&temporary, &bytes).expect("install exact deterministic temp");
        make_private_file(&temporary);

        // An existing exact deterministic temp is not assumed durable.  The
        // injected barrier must be consumed before rename and leave the bytes
        // in place for the next exact retry.
        store.execution_journal.fail_next_file_sync_for_test();
        assert!(matches!(
            store.publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            ),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::Other
        ));
        assert!(store.execution_journal.file_sync_test_faults_consumed());
        assert!(
            store
                .execution_journal
                .existing_temp_rw_opened_for_sync_for_test(),
            "exact deterministic temp must use the RW durability opener"
        );
        assert_eq!(fs::read(&temporary).expect("preserved exact temp"), bytes);
        assert_eq!(sync_journal_temporary_count(&journal), 1);

        store
            .publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            )
            .expect("reused exact temp must sync then publish");
        assert!(!temporary.exists(), "rename consumes the exact staged file");
        assert_eq!(
            store
                .execution_journal
                .held_source_liveness_observations_for_test(),
            0b111,
            "the exact source handle must stay live before/after rename and through final verification"
        );

        let (fixture, store, binding, boundary) = prepared_journal_attempt();
        let journal = fixture.journal_directory();
        store
            .execution_journal
            .fail_before_next_file_sync_for_test();
        assert!(matches!(
            store.publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            ),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::Other
        ));
        assert!(store.execution_journal.file_sync_test_faults_consumed());
        assert_eq!(sync_journal_temporary_count(&journal), 1);
        store
            .publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            )
            .expect("crash-window retry must sync the retained deterministic temp");
    }

    #[test]
    fn pre_witness_timestamp_is_boundary_exact_before_publication_and_bridge_reopen() {
        let (fixture, store, binding, boundary) = prepared_journal_attempt();
        let pre_path = fixture.journal_directory().join(format!(
            "{}-{}.pre",
            boundary.operation_id, boundary.attempt_number
        ));
        assert!(matches!(
            store.publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms + 1,
            ),
            Err(Error::InvalidLocalExecutionEvidence)
        ));
        assert!(
            !pre_path.exists(),
            "a mismatched timestamp must fail before journal file creation"
        );
        drop(store);

        // The same exact-boundary contract remains bridgeable and survives a
        // process boundary; the R3.5 helper publishes the pre witness with
        // the boundary timestamp before finalizing the dependency.
        let bridge_fixture = Fixture::new();
        let (mut bridged, _, _, _, _, _, _, batch_id) = bridged_r3_5_batch(&bridge_fixture);
        bridged
            .commit_r3_change_batch(batch_id, 30)
            .expect("exact-boundary bridge finalizes");
        drop(bridged);
        assert_eq!(
            bridge_fixture
                .open()
                .vault_state()
                .expect("reopened state")
                .expect("bound state")
                .durable_cursor,
            Some("cursor-2".into())
        );
    }

    #[test]
    fn differing_and_truncated_deterministic_temps_are_preserved_and_fail_closed() {
        for staged in [b"different witness".as_slice(), b"MVSEJ".as_slice()] {
            let (fixture, store, binding, boundary) = prepared_journal_attempt();
            let journal = fixture.journal_directory();
            let temporary = journal.join(format!(
                ".sync-execution-witness-{}-{}.pre.tmp",
                boundary.operation_id, boundary.attempt_number
            ));
            let final_path = journal.join(format!(
                "{}-{}.pre",
                boundary.operation_id, boundary.attempt_number
            ));
            fs::write(&temporary, staged).expect("install corrupt deterministic temp");
            make_private_file(&temporary);
            assert!(matches!(
                store.publish_local_execution_pre_side_effect_witness(
                    &binding,
                    &boundary,
                    boundary.occurred_at_unix_ms,
                ),
                Err(Error::LocalExecutionJournalCollision)
            ));
            assert!(!final_path.exists(), "no corrupt temp may become final");
            assert_eq!(fs::read(&temporary).expect("preserved temp"), staged);
            assert_eq!(sync_journal_temporary_count(&journal), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn insecure_symlink_and_hardlink_deterministic_temps_are_preserved() {
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

        for kind in 0..3 {
            let (fixture, store, binding, boundary) = prepared_journal_attempt();
            let journal = fixture.journal_directory();
            let temporary = journal.join(format!(
                ".sync-execution-witness-{}-{}.pre.tmp",
                boundary.operation_id, boundary.attempt_number
            ));
            let final_path = journal.join(format!(
                "{}-{}.pre",
                boundary.operation_id, boundary.attempt_number
            ));
            let source = journal.join(format!("temp-attack-source-{kind}"));
            match kind {
                0 => {
                    fs::write(&temporary, b"insecure temp").expect("insecure temp");
                    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644))
                        .expect("insecure mode");
                }
                1 => {
                    fs::write(&source, b"symlink target").expect("target");
                    make_private_file(&source);
                    symlink(&source, &temporary).expect("temp symlink");
                }
                _ => {
                    fs::write(&source, b"hardlink target").expect("target");
                    make_private_file(&source);
                    fs::hard_link(&source, &temporary).expect("temp hardlink");
                }
            }
            assert!(store
                .publish_local_execution_pre_side_effect_witness(
                    &binding,
                    &boundary,
                    boundary.occurred_at_unix_ms,
                )
                .is_err());
            assert!(!final_path.exists());
            assert_eq!(sync_journal_temporary_count(&journal), 1);
            match kind {
                0 => assert_eq!(
                    fs::metadata(&temporary)
                        .expect("preserved insecure temp")
                        .permissions()
                        .mode()
                        & 0o777,
                    0o644
                ),
                1 => assert!(fs::symlink_metadata(&temporary)
                    .expect("preserved symlink")
                    .file_type()
                    .is_symlink()),
                _ => assert_eq!(
                    fs::metadata(&source).expect("preserved hardlink").nlink(),
                    2
                ),
            }
        }
    }

    #[test]
    fn journal_pathname_substitution_never_becomes_publication_or_read_success() {
        let (fixture, store, binding, boundary) = prepared_journal_attempt();
        let final_path = fixture.journal_directory().join(format!(
            "{}-{}.pre",
            boundary.operation_id, boundary.attempt_number
        ));
        store
            .execution_journal
            .replace_source_before_next_rename_for_test();
        assert!(matches!(
            store.publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            ),
            Err(Error::LocalExecutionJournalMismatch)
        ));
        assert!(store.execution_journal.replacement_test_faults_consumed());
        assert_eq!(
            fs::read(&final_path).expect("substituted final is preserved for forensics"),
            b"journal-test-substitution"
        );

        let (_fixture, store, binding, boundary) = prepared_journal_attempt();
        store
            .publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            )
            .expect("publish exact witness");
        store
            .execution_journal
            .replace_named_file_after_next_read_for_test();
        assert!(matches!(
            store.inspect_local_execution_recovery(&binding, boundary.attempt_number),
            Err(Error::LocalExecutionJournalMismatch)
        ));
        assert!(store.execution_journal.replacement_test_faults_consumed());
    }

    #[test]
    fn journal_rejects_detached_root_topology_before_publication() {
        let fixture = Fixture::new();
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        let mut store = fixture.open();
        store
            .register_local_execution_contract(&binding, 10)
            .expect("contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("boundary");

        let detached = fixture.app_data.with_file_name("detached-private-app-data");
        fs::rename(&fixture.app_data, &detached).expect("detach configured root");
        fs::create_dir(&fixture.app_data).expect("replacement root");
        make_private(&fixture.app_data);

        assert!(matches!(
            store.publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            ),
            Err(Error::LocalExecutionJournalMismatch)
        ));
        assert!(!fixture.journal_directory().exists());
    }

    #[test]
    fn exact_retry_creates_no_temp_and_collision_preserves_final_bytes() {
        let fixture = Fixture::new();
        let operation_id = Uuid::new_v4();
        let binding = test_durable_execution_binding(operation_id, fixture.vault_id);
        let boundary = LocalExecutionAttemptBoundary {
            operation_id,
            attempt_number: 0,
            boundary_id: Uuid::new_v4(),
            contract_fingerprint: binding.fingerprint(),
            occurred_at_unix_ms: 11,
        };
        let mut store = fixture.open();
        store
            .register_local_execution_contract(&binding, 10)
            .expect("contract");
        store
            .append_local_execution_attempt_boundary(&boundary)
            .expect("boundary");
        let journal = fixture.journal_directory();
        let stale = journal.join(".sync-execution-witness-stale.tmp");
        fs::write(&stale, b"pre-existing stale evidence").expect("stale temp");
        make_private_file(&stale);

        store
            .publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            )
            .expect("publish pre");
        let final_path = journal.join(format!("{operation_id}-0.pre"));
        let final_bytes = fs::read(&final_path).expect("final witness");
        let temporary_count_before = sync_journal_temporary_count(&journal);
        assert_eq!(temporary_count_before, 1);
        assert_eq!(
            store
                .publish_local_execution_pre_side_effect_witness(
                    &binding,
                    &boundary,
                    boundary.occurred_at_unix_ms,
                )
                .expect("exact retry"),
            LocalExecutionWitnessPublicationOutcome::AlreadyPublished
        );
        assert_eq!(
            sync_journal_temporary_count(&journal),
            temporary_count_before
        );
        assert_eq!(
            fs::read(&stale).expect("preserved stale temp"),
            b"pre-existing stale evidence"
        );

        assert!(matches!(
            store.publish_local_execution_pre_side_effect_witness(&binding, &boundary, 13),
            Err(Error::InvalidLocalExecutionEvidence)
        ));
        assert_eq!(fs::read(final_path).expect("preserved final"), final_bytes);
    }

    #[test]
    fn malformed_unsupported_and_oversized_witness_bytes_are_preserved_on_disk() {
        for corruption in 0..3 {
            let (fixture, store, binding, boundary) = prepared_journal_attempt();
            store
                .publish_local_execution_pre_side_effect_witness(
                    &binding,
                    &boundary,
                    boundary.occurred_at_unix_ms,
                )
                .expect("publish valid witness");
            let final_path = fixture
                .journal_directory()
                .join(format!("{}-0.pre", boundary.operation_id));
            let valid = fs::read(&final_path).expect("valid witness bytes");
            let corrupted = match corruption {
                0 => valid[..valid.len() - 1].to_vec(),
                1 => {
                    let mut unsupported = valid;
                    unsupported[6] = unsupported[6].wrapping_add(1);
                    unsupported
                }
                _ => vec![0; 513],
            };
            fs::write(&final_path, &corrupted).expect("install corrupt witness");
            assert!(matches!(
                store.inspect_local_execution_recovery(&binding, 0),
                Err(Error::LocalExecutionJournalMalformed)
            ));
            assert_eq!(
                fs::read(&final_path).expect("preserved corrupt witness"),
                corrupted
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hardlink_and_insecure_final_witnesses_fail_closed_and_are_preserved() {
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

        let (fixture, store, binding, boundary) = prepared_journal_attempt();
        let journal = fixture.journal_directory();
        let final_path = journal.join(format!("{}-0.pre", boundary.operation_id));
        let symlink_target = journal.join("attacker-target");
        fs::write(&symlink_target, b"attacker bytes").expect("symlink target");
        make_private_file(&symlink_target);
        symlink(&symlink_target, &final_path).expect("install final symlink");
        assert!(store
            .publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            )
            .is_err());
        assert!(fs::symlink_metadata(&final_path)
            .expect("preserved symlink")
            .file_type()
            .is_symlink());

        let (fixture, store, binding, boundary) = prepared_journal_attempt();
        let journal = fixture.journal_directory();
        let final_path = journal.join(format!("{}-0.pre", boundary.operation_id));
        let hardlink_source = journal.join("attacker-hardlink-source");
        fs::write(&hardlink_source, b"attacker bytes").expect("hardlink source");
        make_private_file(&hardlink_source);
        fs::hard_link(&hardlink_source, &final_path).expect("install final hardlink");
        assert!(store
            .publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            )
            .is_err());
        assert_eq!(
            fs::metadata(&hardlink_source)
                .expect("preserved hardlink")
                .nlink(),
            2
        );

        let (fixture, store, binding, boundary) = prepared_journal_attempt();
        let final_path = fixture
            .journal_directory()
            .join(format!("{}-0.pre", boundary.operation_id));
        fs::write(&final_path, b"attacker bytes").expect("insecure final");
        fs::set_permissions(&final_path, fs::Permissions::from_mode(0o644)).expect("insecure mode");
        assert!(store
            .publish_local_execution_pre_side_effect_witness(
                &binding,
                &boundary,
                boundary.occurred_at_unix_ms,
            )
            .is_err());
        assert_eq!(
            fs::metadata(&final_path)
                .expect("preserved insecure final")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[test]
    fn local_execution_v5_to_v6_migration_is_additive_and_empty() {
        let fixture = Fixture::new();
        let mut store = fixture.open();
        let binding =
            VerifiedRemoteBinding::new("account-a", "remote-root", "account-a", "remote-root")
                .expect("binding");
        store
            .bind_remote_root(&binding, 7)
            .expect("persist v5 binding fact");
        let job = QueueJob::new(
            Uuid::new_v4(),
            QueueJobKind::Upload,
            "preserved.md",
            None,
            None,
            None,
            8,
        )
        .expect("queue job");
        store.enqueue_job(&job).expect("persist v5 queue fact");
        let expected_state = store.vault_state().expect("state");
        let database_path = store.database_path().to_owned();
        drop(store);
        let connection = Connection::open(&database_path).expect("migration fixture");
        for (kind, name, _) in LOCAL_EXECUTION_SCHEMA_OBJECTS.iter().rev() {
            connection
                .execute_batch(&format!("DROP {} {name};", kind.to_uppercase()))
                .expect("drop v6 fixture object");
        }
        connection
            .pragma_update(None, "user_version", 5)
            .expect("downgrade fixture");
        drop(connection);
        let migrated = fixture.open();
        assert_eq!(migrated.schema_version().expect("schema version"), 6);
        assert_eq!(
            migrated.vault_state().expect("migrated state"),
            expected_state
        );
        assert_eq!(
            migrated.job(job.operation_id()).expect("migrated job"),
            Some(job)
        );
        for table in [
            "local_execution_contracts",
            "local_execution_identity_evidence",
            "local_execution_collision_members",
            "local_execution_contract_completions",
            "local_execution_attempt_boundaries",
            "local_execution_attempt_outcomes",
        ] {
            assert_eq!(count(migrated.database_path(), table), 0, "{table}");
        }
    }
}
