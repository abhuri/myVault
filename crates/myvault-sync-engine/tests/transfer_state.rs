use myvault_sync_engine::{
    BindOutcome, ChangeBatchDependency, ChangeBatchDependencyKind, ChangesPage, ConflictEvidence,
    ConflictEvidenceRegistrationOutcome, EnqueueOutcome, Error, JobState, MutationDisposition,
    MutationEvidenceCapturePhase, MutationIntent, MutationOperationKind, MutationOutcomeTransition,
    MutationPhase, MutationRegistrationOutcome, MutationVerificationEvidence, QueueJob,
    QueueJobKind, RemoteContentHash, RemoteEntry, RemoteEntryKind, RemoteHashAlgorithm, ScanPage,
    SyncStore, TransferCompletion, TransferCompletionOutcome, TransferDirection, TransferMimeClass,
    TransferPhase, TransferRecord, TransferRegistrationOutcome, VerifiedRemoteBinding,
    SCHEMA_VERSION,
};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    _temp: TempDir,
    app_data: PathBuf,
    vault: PathBuf,
    vault_id: Uuid,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp root");
        let root = temp.path().canonicalize().expect("canonical temp root");
        let app_data = root.join("private-app-data");
        let vault = root.join("Vault");
        fs::create_dir(&app_data).expect("app data");
        fs::create_dir(&vault).expect("vault");
        make_private(&app_data);
        Self {
            _temp: temp,
            app_data,
            vault,
            vault_id: Uuid::new_v4(),
        }
    }

    fn open(&self) -> SyncStore {
        SyncStore::open(&self.app_data, &self.vault, self.vault_id).expect("sync store")
    }
}

#[cfg(unix)]
fn make_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private mode");
}

#[cfg(not(unix))]
fn make_private(_path: &Path) {}

fn binding() -> VerifiedRemoteBinding {
    VerifiedRemoteBinding::new("account-a", "remote-root", "account-a", "remote-root").unwrap()
}

fn bound_store(fixture: &Fixture) -> SyncStore {
    let mut store = fixture.open();
    assert_eq!(
        store.bind_remote_root(&binding(), 1).unwrap(),
        BindOutcome::Created
    );
    store
}

fn cursor_ready_store(fixture: &Fixture) -> SyncStore {
    let mut store = bound_store(fixture);
    store.begin_initial_scan("start-token", 2).unwrap();
    store
        .apply_scan_page(
            None,
            &ScanPage {
                entries: Vec::new(),
                next_page_token: None,
            },
            3,
        )
        .unwrap();
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
        .unwrap();
    store
}

fn hash(byte: u8) -> String {
    std::iter::repeat_n(char::from(byte), 64).collect()
}

fn mutation_intent(operation_id: Uuid, marker: &str) -> MutationIntent {
    let mut intent = MutationIntent {
        operation_id,
        operation_kind: MutationOperationKind::LocalPublish,
        account_id: None,
        remote_root_id: None,
        remote_file_id: None,
        source_parent_id: None,
        destination_parent_id: None,
        local_object_id: None,
        source_path: Some("notes/mutation.md".into()),
        destination_path: None,
        expected_local_revision: Some("revision-a".into()),
        expected_remote_revision: None,
        base_reference: None,
        base_local_revision: None,
        base_remote_revision: None,
        base_sha256: None,
        base_byte_length: None,
        expected_local_sha256: Some(hash(b'a')),
        expected_local_byte_length: Some(1),
        expected_remote_sha256: None,
        expected_remote_byte_length: None,
        operation_marker: marker.into(),
        intent_fingerprint: String::new(),
        registered_at_unix_ms: 10,
    };
    intent.intent_fingerprint = intent.canonical_fingerprint();
    intent
}

fn mutation_evidence(
    evidence_id: Uuid,
    operation_id: Uuid,
    attempt_number: u32,
    disposition: MutationDisposition,
    outcome_code: Option<&str>,
    operation_marker: &str,
) -> MutationVerificationEvidence {
    let mut evidence = MutationVerificationEvidence {
        evidence_id,
        operation_id,
        attempt_number,
        capture_phase: MutationEvidenceCapturePhase::Reconcile,
        disposition,
        outcome_code: outcome_code.map(str::to_owned),
        observed_account_id: None,
        observed_remote_root_id: None,
        observed_remote_file_id: None,
        observed_parent_id: None,
        observed_path: Some("notes/mutation.md".into()),
        observed_local_revision: Some("revision-a".into()),
        observed_remote_revision: None,
        observed_sha256: Some(hash(b'a')),
        observed_byte_length: Some(1),
        observed_operation_marker: Some(operation_marker.into()),
        forbidden_side_effect: false,
        verified_received_byte_offset: None,
        resume_reference: None,
        evidence_fingerprint: String::new(),
        captured_at_unix_ms: 20,
    };
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    evidence
}

fn complete_registered_r3_mutation(
    store: &mut SyncStore,
    operation_id: Uuid,
    operation_marker: &str,
) -> Uuid {
    store.claim_mutation(operation_id, 0, 11).unwrap();
    let evidence_id = Uuid::new_v4();
    let mut evidence = mutation_evidence(
        evidence_id,
        operation_id,
        0,
        MutationDisposition::VerifiedApplied,
        Some("verified_applied"),
        operation_marker,
    );
    evidence.capture_phase = MutationEvidenceCapturePhase::PostVerify;
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    store
        .record_mutation_outcome(
            operation_id,
            1,
            &evidence,
            &MutationOutcomeTransition::VerifiedApplied,
        )
        .unwrap();
    evidence_id
}

fn conflict_evidence(conflict_id: &str, operation_id: Uuid) -> ConflictEvidence {
    let mut evidence = ConflictEvidence {
        conflict_id: conflict_id.into(),
        operation_id,
        stable_cell_id: "cell-a".into(),
        local_state_code: "changed".into(),
        remote_state_code: "changed".into(),
        content_class: "text".into(),
        lineage_state: "known".into(),
        classification_code: "needs_reconcile".into(),
        ambiguity_reason: "overlap".into(),
        evidence_sufficiency: "complete".into(),
        conflict_copy_operation_id: None,
        base_evidence_id: None,
        local_evidence_id: None,
        remote_evidence_id: None,
        base_sha256: Some(hash(b'a')),
        base_byte_length: Some(1),
        local_sha256: Some(hash(b'b')),
        local_byte_length: Some(1),
        remote_sha256: Some(hash(b'c')),
        remote_byte_length: Some(1),
        naming_version: "v1".into(),
        normalized_collision_key: "cell-a".into(),
        target_parent_id: "parent-a".into(),
        expected_conflict_copy_sha256: None,
        expected_conflict_copy_byte_length: None,
        explanation_code: Some("overlap".into()),
        device_alias: Some("device-a".into()),
        evidence_fingerprint: String::new(),
        captured_at_unix_ms: 20,
    };
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    evidence
}

#[test]
fn exact_remote_entry_lookup_preserves_transfer_evidence() {
    let fixture = Fixture::new();
    let mut store = bound_store(&fixture);
    store.begin_initial_scan("start-token", 2).unwrap();
    let expected = RemoteEntry {
        file_id: "remote-file".into(),
        parent_id: "remote-root".into(),
        path: "ภาษาไทย.bin".into(),
        kind: RemoteEntryKind::File,
        content_hash: Some(
            RemoteContentHash::new(RemoteHashAlgorithm::Sha256, hash(b'a')).unwrap(),
        ),
        remote_revision: hash(b'b'),
    };
    store
        .apply_scan_page(
            None,
            &ScanPage {
                entries: vec![expected.clone()],
                next_page_token: None,
            },
            3,
        )
        .unwrap();

    assert_eq!(store.remote_entry("remote-file").unwrap(), Some(expected));
    assert!(store.remote_entry("missing-file").unwrap().is_none());
    assert!(store.remote_entry("../invalid").is_err());
}

fn upload(operation_id: Uuid, marker: &str) -> TransferRecord {
    TransferRecord::new(
        operation_id,
        TransferDirection::Upload,
        "notes/hello.md",
        "remote-parent",
        None,
        Some(hash(b'a')),
        None,
        hash(b'b'),
        42,
        TransferMimeClass::Markdown,
        marker,
        Some("stage.abcdef".into()),
        None,
        10,
    )
    .unwrap()
}

fn completion() -> TransferCompletion {
    TransferCompletion::new(
        "remote-file",
        "remote-revision-1",
        hash(b'a'),
        "base.abcdef",
        "uploaded_verified",
        40,
    )
    .unwrap()
}

fn downgrade_to_v2(database_path: &Path) {
    let connection = rusqlite::Connection::open(database_path).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TRIGGER conflict_evidence_no_delete;
             DROP TRIGGER conflict_evidence_no_update;
             DROP TRIGGER mutation_evidence_no_delete;
             DROP TRIGGER mutation_evidence_no_update;
             DROP TRIGGER mutation_events_no_delete;
             DROP TRIGGER mutation_events_no_update;
             DROP TRIGGER mutation_intents_no_delete;
             DROP TRIGGER mutation_intents_no_update;
             DROP INDEX conflict_evidence_copy_idx;
             DROP INDEX conflict_evidence_stable_cell_idx;
             DROP INDEX mutation_evidence_operation_attempt_idx;
             DROP INDEX mutation_events_operation_attempt_idx;
             DROP INDEX mutation_state_claim_idx;
             DROP TABLE change_batch_mutations;
             DROP TABLE conflict_evidence;
             DROP TABLE mutation_events;
             DROP TABLE mutation_state;
             DROP TABLE mutation_verification_evidence;
             DROP TABLE mutation_intents;
             CREATE TABLE change_batch_mutations (
                batch_id TEXT NOT NULL,
                mutation_id TEXT NOT NULL,
                state TEXT NOT NULL CHECK (state IN ('pending', 'applying', 'committed')),
                PRIMARY KEY (batch_id, mutation_id),
                FOREIGN KEY (batch_id) REFERENCES change_batch(batch_id) ON DELETE CASCADE
             );
             DROP TABLE transfer_history;
             DROP INDEX transfers_due_idx;
             DROP TABLE transfers;
             PRAGMA user_version = 2;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
}

fn downgrade_to_v3(database_path: &Path) {
    let connection = rusqlite::Connection::open(database_path).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TRIGGER conflict_evidence_no_delete;
             DROP TRIGGER conflict_evidence_no_update;
             DROP TRIGGER mutation_evidence_no_delete;
             DROP TRIGGER mutation_evidence_no_update;
             DROP TRIGGER mutation_events_no_delete;
             DROP TRIGGER mutation_events_no_update;
             DROP TRIGGER mutation_intents_no_delete;
             DROP TRIGGER mutation_intents_no_update;
             DROP INDEX conflict_evidence_copy_idx;
             DROP INDEX conflict_evidence_stable_cell_idx;
             DROP INDEX mutation_evidence_operation_attempt_idx;
             DROP INDEX mutation_events_operation_attempt_idx;
             DROP INDEX mutation_state_claim_idx;
             ALTER TABLE change_batch_mutations RENAME TO change_batch_mutations_v4;
             CREATE TABLE change_batch_mutations (
                batch_id TEXT NOT NULL,
                mutation_id TEXT NOT NULL,
                state TEXT NOT NULL CHECK (state IN ('pending', 'applying', 'committed')),
                PRIMARY KEY (batch_id, mutation_id),
                FOREIGN KEY (batch_id) REFERENCES change_batch(batch_id) ON DELETE CASCADE
             );
             INSERT INTO change_batch_mutations(batch_id, mutation_id, state)
             SELECT batch_id, mutation_id,
                    CASE state WHEN 'needs_reconcile' THEN 'applying' ELSE state END
             FROM change_batch_mutations_v4;
             DROP TABLE change_batch_mutations_v4;
             DROP TABLE conflict_evidence;
             DROP TABLE mutation_events;
             DROP TABLE mutation_state;
             DROP TABLE mutation_verification_evidence;
             DROP TABLE mutation_intents;
             PRAGMA user_version = 3;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
}

#[test]
fn v2_migration_preserves_r1_binding_cursor_metadata_queue_and_history() {
    let fixture = Fixture::new();
    let database_path;
    let completed_job = Uuid::new_v4();
    {
        let mut store = bound_store(&fixture);
        store.begin_initial_scan("start-1", 2).unwrap();
        store
            .apply_scan_page(
                None,
                &ScanPage {
                    entries: vec![RemoteEntry {
                        file_id: "remote-file".into(),
                        parent_id: "remote-root".into(),
                        path: "hello.md".into(),
                        kind: RemoteEntryKind::File,
                        content_hash: Some(
                            RemoteContentHash::new(RemoteHashAlgorithm::Sha256, hash(b'c'))
                                .unwrap(),
                        ),
                        remote_revision: "remote-revision-1".into(),
                    }],
                    next_page_token: None,
                },
                3,
            )
            .unwrap();
        store
            .apply_changes_page(
                "start-1",
                &myvault_sync_engine::ChangesPage {
                    changes: Vec::new(),
                    next_page_token: None,
                    new_start_page_token: Some("durable-1".into()),
                },
                4,
            )
            .unwrap();
        let job = QueueJob::new(
            completed_job,
            QueueJobKind::Upload,
            "hello.md",
            None,
            None,
            Some(hash(b'd')),
            5,
        )
        .unwrap();
        assert_eq!(store.enqueue_job(&job).unwrap(), EnqueueOutcome::Enqueued);
        assert_eq!(
            store.claim_next_job(5).unwrap().unwrap().state(),
            JobState::Running
        );
        store
            .complete_verified_job(completed_job, "verified", 6)
            .unwrap();
        database_path = store.database_path().to_owned();
    }

    downgrade_to_v2(&database_path);
    let store = fixture.open();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let state = store.vault_state().unwrap().unwrap();
    assert_eq!(state.account_id.as_deref(), Some("account-a"));
    assert_eq!(state.durable_cursor.as_deref(), Some("durable-1"));
    assert_eq!(store.remote_entry_count().unwrap(), 1);
    assert_eq!(store.queue_count().unwrap(), 0);
    assert_eq!(store.history_count().unwrap(), 1);
    assert_eq!(store.transfer_count().unwrap(), 0);
    assert_eq!(
        store.job(completed_job).unwrap().unwrap().state(),
        JobState::Completed
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn v3_to_v4_migration_preserves_legacy_queue_and_blocks_cursor_without_fabricating_evidence() {
    let fixture = Fixture::new();
    let database_path;
    let batch_id = Uuid::new_v4();
    let move_id = Uuid::new_v4();
    let trash_id = Uuid::new_v4();
    {
        let mut store = bound_store(&fixture);
        store.begin_initial_scan("start-1", 2).unwrap();
        store
            .apply_scan_page(
                None,
                &ScanPage {
                    entries: Vec::new(),
                    next_page_token: None,
                },
                3,
            )
            .unwrap();
        store
            .apply_changes_page(
                "start-1",
                &myvault_sync_engine::ChangesPage {
                    changes: Vec::new(),
                    next_page_token: None,
                    new_start_page_token: Some("cursor-1".into()),
                },
                4,
            )
            .unwrap();
        let move_job = QueueJob::new(
            move_id,
            QueueJobKind::Move,
            "notes/a.md",
            Some("archive/a.md".into()),
            Some("remote-a".into()),
            None,
            4,
        )
        .unwrap();
        let trash_job = QueueJob::new(
            trash_id,
            QueueJobKind::Trash,
            "notes/b.md",
            None,
            Some("remote-b".into()),
            None,
            5,
        )
        .unwrap();
        store.enqueue_job(&move_job).unwrap();
        store.enqueue_job(&trash_job).unwrap();
        store
            .begin_change_batch(batch_id, "cursor-1", "cursor-2", ["legacy-write"])
            .unwrap();
        database_path = store.database_path().to_owned();
    }

    downgrade_to_v3(&database_path);
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
    drop(connection);

    let mut store = fixture.open();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(
        store.job(move_id).unwrap().unwrap().kind(),
        QueueJobKind::Move
    );
    assert_eq!(
        store.job(trash_id).unwrap().unwrap().kind(),
        QueueJobKind::Trash
    );
    assert!(matches!(
        store.commit_change_batch(batch_id, 6),
        Err(Error::LocalMutationIncomplete)
    ));
    assert_eq!(
        store
            .vault_state()
            .unwrap()
            .unwrap()
            .durable_cursor
            .as_deref(),
        Some("cursor-1")
    );

    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let dependency: (String, Option<String>, Option<String>, String) = connection
        .query_row(
            "SELECT dependency_kind, operation_id, committed_evidence_id, state
             FROM change_batch_mutations WHERE batch_id = ?1",
            [batch_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(dependency.0, "legacy_v3");
    assert_eq!(dependency.1, None);
    assert_eq!(dependency.2, None);
    assert_eq!(dependency.3, "pending");
    for table in [
        "mutation_intents",
        "mutation_state",
        "mutation_events",
        "mutation_verification_evidence",
        "conflict_evidence",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "migration fabricated rows in {table}");
    }
    let triggers: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'trigger' AND name IN (
                'mutation_intents_no_update', 'mutation_intents_no_delete',
                'mutation_events_no_update', 'mutation_events_no_delete',
                'mutation_evidence_no_update', 'mutation_evidence_no_delete',
                'conflict_evidence_no_update', 'conflict_evidence_no_delete'
             )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(triggers, 8);

    connection
        .execute(
            "INSERT INTO mutation_intents(
                operation_id, operation_kind, operation_marker, intent_fingerprint,
                registered_at_unix_ms
             ) VALUES ('intent-1', 'local_publish', 'marker-1', 'fingerprint-1', 0)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO mutation_verification_evidence(
                evidence_id, operation_id, attempt_number, capture_phase, disposition,
                forbidden_side_effect, evidence_fingerprint, captured_at_unix_ms
             ) VALUES ('evidence-1', 'intent-1', 0, 'preflight', 'needs_reconcile',
                       1, 'evidence-fingerprint-1', 0)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO mutation_events(
                event_id, operation_id, attempt_number, state_version, phase,
                occurred_at_unix_ms
             ) VALUES (1, 'intent-1', 0, 0, 'intent_durable', 0)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO conflict_evidence(
                conflict_id, operation_id, stable_cell_id, local_state_code,
                remote_state_code, content_class, lineage_state, classification_code,
                ambiguity_reason, evidence_sufficiency, naming_version,
                normalized_collision_key, target_parent_id, evidence_fingerprint,
                captured_at_unix_ms
             ) VALUES (
                'conflict-1', 'intent-1', 'cell-1', 'changed', 'changed', 'text',
                'known', 'needs_reconcile', 'none', 'complete', 'v1', 'cell-1',
                'parent-1', 'conflict-fingerprint-1', 0
             )",
            [],
        )
        .unwrap();
    for (table, predicate) in [
        ("mutation_intents", "operation_id = 'intent-1'"),
        (
            "mutation_verification_evidence",
            "evidence_id = 'evidence-1'",
        ),
        ("mutation_events", "event_id = 1"),
        ("conflict_evidence", "conflict_id = 'conflict-1'"),
    ] {
        assert!(connection
            .execute(&format!("UPDATE {table} SET {predicate}"), [])
            .is_err());
        assert!(connection
            .execute(&format!("DELETE FROM {table} WHERE {predicate}"), [])
            .is_err());
    }
}

#[test]
fn mutation_ledger_is_versioned_immutable_and_recovers_running_outcomes() {
    let fixture = Fixture::new();
    let operation_id = Uuid::new_v4();
    let intent = mutation_intent(operation_id, "mutation-marker");
    let database_path;
    {
        let mut store = fixture.open();
        assert_eq!(
            store.register_mutation_intent(&intent, None).unwrap(),
            MutationRegistrationOutcome::Registered
        );
        assert_eq!(
            store.register_mutation_intent(&intent, None).unwrap(),
            MutationRegistrationOutcome::AlreadyPresent
        );
        let mut collision = mutation_intent(Uuid::new_v4(), "mutation-marker");
        collision.intent_fingerprint = collision.canonical_fingerprint();
        assert!(matches!(
            store.register_mutation_intent(&collision, None),
            Err(Error::MutationCollision)
        ));
        let initial = store.mutation_state(operation_id).unwrap().unwrap();
        assert_eq!(initial.phase, MutationPhase::IntentDurable);
        assert_eq!(initial.state_version, 0);
        assert!(matches!(
            store.claim_mutation(operation_id, 1, 11),
            Err(Error::MutationStateVersionMismatch)
        ));
        let running = store.claim_mutation(operation_id, 0, 11).unwrap();
        assert_eq!(running.phase, MutationPhase::Running);
        assert_eq!(running.state_version, 1);
        assert_eq!(store.mutation_events(operation_id).unwrap().len(), 2);
        database_path = store.database_path().to_owned();
    }

    let mut recovered = fixture.open();
    let recovered_state = recovered.mutation_state(operation_id).unwrap().unwrap();
    assert_eq!(recovered_state.phase, MutationPhase::NeedsReconcile);
    assert_eq!(
        recovered_state.disposition,
        Some(MutationDisposition::NeedsReconcile)
    );
    assert_eq!(recovered_state.state_version, 2);
    assert!(recovered_state.last_evidence_id.is_some());
    assert_eq!(
        recovered_state.outcome_code.as_deref(),
        Some("interrupted_unknown_outcome")
    );
    assert!(matches!(
        recovered.claim_mutation(operation_id, 2, 12),
        Err(Error::InvalidStateTransition)
    ));
    let events = recovered.mutation_events(operation_id).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events.last().unwrap().evidence_id,
        recovered_state.last_evidence_id
    );
    assert_eq!(
        events.last().unwrap().outcome_code.as_deref(),
        Some("interrupted_unknown_outcome")
    );
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let outcome_code: Option<String> = connection
        .query_row(
            "SELECT outcome_code FROM mutation_verification_evidence
             WHERE evidence_id = ?1",
            [recovered_state.last_evidence_id.unwrap().to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outcome_code.as_deref(), Some("interrupted_unknown_outcome"));
}

#[test]
fn mutation_outcome_binds_evidence_event_and_state_atomically() {
    let fixture = Fixture::new();
    let operation_id = Uuid::new_v4();
    let evidence_id = Uuid::new_v4();
    let mut store = fixture.open();
    store
        .register_mutation_intent(&mutation_intent(operation_id, "applied-marker"), None)
        .unwrap();
    store.claim_mutation(operation_id, 0, 11).unwrap();
    let evidence = mutation_evidence(
        evidence_id,
        operation_id,
        0,
        MutationDisposition::VerifiedApplied,
        Some("verified_applied"),
        "applied-marker",
    );
    let mut evidence = evidence;
    evidence.capture_phase = MutationEvidenceCapturePhase::PostVerify;
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    let completed = store
        .record_mutation_outcome(
            operation_id,
            1,
            &evidence,
            &MutationOutcomeTransition::VerifiedApplied,
        )
        .unwrap();
    assert_eq!(completed.phase, MutationPhase::Completed);
    assert_eq!(completed.state_version, 2);
    assert_eq!(completed.last_evidence_id, Some(evidence_id));
    assert_eq!(completed.outcome_code.as_deref(), Some("verified_applied"));
    assert!(matches!(
        store.record_mutation_outcome(
            operation_id,
            2,
            &evidence,
            &MutationOutcomeTransition::VerifiedApplied,
        ),
        Err(Error::InvalidStateTransition)
    ));
    let events = store.mutation_events(operation_id).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events.last().unwrap().evidence_id, Some(evidence_id));
    assert_eq!(
        events.last().unwrap().outcome_code.as_deref(),
        Some("verified_applied")
    );
}

#[test]
fn canonical_fingerprints_reject_field_drift_and_caller_forgery() {
    let fixture = Fixture::new();
    let operation_id = Uuid::new_v4();
    let intent = mutation_intent(operation_id, "fingerprint-marker");
    let mut store = fixture.open();
    store.register_mutation_intent(&intent, None).unwrap();

    let mut drifted = intent.clone();
    drifted.destination_path = Some("notes/other.md".into());
    assert!(matches!(
        store.register_mutation_intent(&drifted, None),
        Err(Error::InvalidTransferEvidence)
    ));

    store.claim_mutation(operation_id, 0, 11).unwrap();
    let mut forged = mutation_evidence(
        Uuid::new_v4(),
        operation_id,
        0,
        MutationDisposition::VerifiedApplied,
        Some("verified_applied"),
        "fingerprint-marker",
    );
    forged.capture_phase = MutationEvidenceCapturePhase::PostVerify;
    forged.observed_path = Some("notes/forged.md".into());
    assert!(matches!(
        store.record_mutation_outcome(
            operation_id,
            1,
            &forged,
            &MutationOutcomeTransition::VerifiedApplied,
        ),
        Err(Error::InvalidTransferEvidence)
    ));
    let state = store.mutation_state(operation_id).unwrap().unwrap();
    assert_eq!(state.phase, MutationPhase::Running);
    assert_eq!(state.state_version, 1);
}

#[test]
fn verified_applied_requires_the_destination_path_when_present() {
    let fixture = Fixture::new();
    let operation_id = Uuid::new_v4();
    let mut intent = mutation_intent(operation_id, "destination-marker");
    intent.destination_path = Some("notes/destination.md".into());
    intent.intent_fingerprint = intent.canonical_fingerprint();
    let mut store = fixture.open();
    store.register_mutation_intent(&intent, None).unwrap();
    store.claim_mutation(operation_id, 0, 11).unwrap();

    let mut evidence = mutation_evidence(
        Uuid::new_v4(),
        operation_id,
        0,
        MutationDisposition::VerifiedApplied,
        Some("verified_applied"),
        "destination-marker",
    );
    evidence.capture_phase = MutationEvidenceCapturePhase::PostVerify;
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    assert!(matches!(
        store.record_mutation_outcome(
            operation_id,
            1,
            &evidence,
            &MutationOutcomeTransition::VerifiedApplied,
        ),
        Err(Error::InvalidTransferEvidence)
    ));

    evidence.observed_path = intent.destination_path.clone();
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    assert_eq!(
        store
            .record_mutation_outcome(
                operation_id,
                1,
                &evidence,
                &MutationOutcomeTransition::VerifiedApplied,
            )
            .unwrap()
            .phase,
        MutationPhase::Completed
    );
}

#[test]
fn conflict_envelope_is_immutable_idempotent_and_excludes_explanatory_metadata() {
    let fixture = Fixture::new();
    let operation_id = Uuid::new_v4();
    let mut store = fixture.open();
    store
        .register_mutation_intent(&mutation_intent(operation_id, "conflict-envelope"), None)
        .unwrap();
    let evidence = conflict_evidence("conflict-a", operation_id);
    assert_eq!(
        store.record_conflict_evidence(&evidence).unwrap(),
        ConflictEvidenceRegistrationOutcome::Registered
    );
    assert_eq!(
        store.conflict_evidence("conflict-a").unwrap(),
        Some(evidence.clone())
    );

    let mut explanatory_rerun = evidence.clone();
    explanatory_rerun.device_alias = Some("device-b".into());
    explanatory_rerun.captured_at_unix_ms = 99;
    assert_eq!(
        store.record_conflict_evidence(&explanatory_rerun).unwrap(),
        ConflictEvidenceRegistrationOutcome::AlreadyPresent
    );

    let mut forged = evidence;
    forged.classification_code = "other".into();
    assert!(matches!(
        store.record_conflict_evidence(&forged),
        Err(Error::InvalidTransferEvidence)
    ));

    let non_copy_operation_id = Uuid::new_v4();
    store
        .register_mutation_intent(
            &mutation_intent(non_copy_operation_id, "not-conflict-copy"),
            None,
        )
        .unwrap();
    let mut invalid_copy = conflict_evidence("conflict-b", operation_id);
    invalid_copy.conflict_copy_operation_id = Some(non_copy_operation_id);
    invalid_copy.expected_conflict_copy_sha256 = Some(hash(b'd'));
    invalid_copy.expected_conflict_copy_byte_length = Some(1);
    invalid_copy.evidence_fingerprint = invalid_copy.canonical_fingerprint();
    assert!(matches!(
        store.record_conflict_evidence(&invalid_copy),
        Err(Error::MutationCollision)
    ));
}

#[test]
fn cursor_rejects_a_completion_event_with_a_semantic_version_mismatch() {
    let fixture = Fixture::new();
    let operation_id = Uuid::new_v4();
    let dependency = ChangeBatchDependency {
        operation_id,
        kind: ChangeBatchDependencyKind::Mutation,
    };
    let batch_id = Uuid::new_v4();
    let database_path;
    let evidence_id;
    {
        let mut store = cursor_ready_store(&fixture);
        store
            .register_mutation_intent(&mutation_intent(operation_id, "event-version"), None)
            .unwrap();
        store
            .begin_r3_change_batch(batch_id, "cursor-1", "cursor-2", &[dependency])
            .unwrap();
        evidence_id = complete_registered_r3_mutation(&mut store, operation_id, "event-version");
        database_path = store.database_path().to_owned();
        drop(store);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
            .execute_batch("DROP TRIGGER mutation_events_no_update;")
            .unwrap();
        connection
            .execute(
                "UPDATE mutation_events SET state_version = 99 WHERE evidence_id = ?1",
                [evidence_id.to_string()],
            )
            .unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER mutation_events_no_update
                 BEFORE UPDATE ON mutation_events BEGIN SELECT RAISE(ABORT, 'mutation_events_immutable'); END",
            )
            .unwrap();
    }
    let mut reopened =
        SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id).unwrap();
    assert!(matches!(
        reopened.commit_r3_change_dependency(batch_id, dependency, evidence_id),
        Err(Error::LocalMutationIncomplete)
    ));
    assert!(matches!(
        reopened.commit_r3_change_batch(batch_id, 21),
        Err(Error::LocalMutationIncomplete)
    ));
}

#[test]
fn verified_applied_requires_post_verify_evidence_bound_to_the_immutable_intent() {
    let fixture = Fixture::new();
    let operation_id = Uuid::new_v4();
    let mut store = fixture.open();
    store
        .register_mutation_intent(&mutation_intent(operation_id, "exact-marker"), None)
        .unwrap();
    store.claim_mutation(operation_id, 0, 11).unwrap();
    let mut wrong_path = mutation_evidence(
        Uuid::new_v4(),
        operation_id,
        0,
        MutationDisposition::VerifiedApplied,
        Some("verified_applied"),
        "exact-marker",
    );
    wrong_path.capture_phase = MutationEvidenceCapturePhase::PostVerify;
    wrong_path.observed_path = Some("notes/other.md".into());
    wrong_path.evidence_fingerprint = wrong_path.canonical_fingerprint();
    assert!(matches!(
        store.record_mutation_outcome(
            operation_id,
            1,
            &wrong_path,
            &MutationOutcomeTransition::VerifiedApplied,
        ),
        Err(Error::InvalidTransferEvidence)
    ));

    let mut wrong_marker = mutation_evidence(
        Uuid::new_v4(),
        operation_id,
        0,
        MutationDisposition::VerifiedApplied,
        Some("verified_applied"),
        "different-marker",
    );
    wrong_marker.capture_phase = MutationEvidenceCapturePhase::PostVerify;
    wrong_marker.evidence_fingerprint = wrong_marker.canonical_fingerprint();
    assert!(matches!(
        store.record_mutation_outcome(
            operation_id,
            1,
            &wrong_marker,
            &MutationOutcomeTransition::VerifiedApplied,
        ),
        Err(Error::InvalidTransferEvidence)
    ));
    let state = store.mutation_state(operation_id).unwrap().unwrap();
    assert_eq!(state.phase, MutationPhase::Running);
    assert_eq!(state.state_version, 1);
    assert_eq!(store.mutation_events(operation_id).unwrap().len(), 2);
}

#[test]
fn r3_1_rejects_retry_scheduled_outcomes_without_exact_revalidation() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    let not_applied_id = Uuid::new_v4();
    store
        .register_mutation_intent(&mutation_intent(not_applied_id, "not-applied-marker"), None)
        .unwrap();
    store.claim_mutation(not_applied_id, 0, 11).unwrap();
    let not_applied = mutation_evidence(
        Uuid::new_v4(),
        not_applied_id,
        0,
        MutationDisposition::VerifiedNotApplied,
        Some("verified_not_applied"),
        "not-applied-marker",
    );
    assert!(matches!(
        store.record_mutation_outcome(
            not_applied_id,
            1,
            &not_applied,
            &MutationOutcomeTransition::VerifiedNotApplied {
                next_attempt_at_unix_ms: 20,
            },
        ),
        Err(Error::InvalidStateTransition)
    ));

    let retry_safe_id = Uuid::new_v4();
    store
        .register_mutation_intent(&mutation_intent(retry_safe_id, "retry-safe-marker"), None)
        .unwrap();
    store.claim_mutation(retry_safe_id, 0, 11).unwrap();
    let mut retry_safe = mutation_evidence(
        Uuid::new_v4(),
        retry_safe_id,
        0,
        MutationDisposition::RetrySafe,
        Some("retry_safe"),
        "retry-safe-marker",
    );
    retry_safe.resume_reference = Some("resume.abcdef".into());
    retry_safe.verified_received_byte_offset = Some(0);
    retry_safe.evidence_fingerprint = retry_safe.canonical_fingerprint();
    assert!(matches!(
        store.record_mutation_outcome(
            retry_safe_id,
            1,
            &retry_safe,
            &MutationOutcomeTransition::RetrySafe {
                next_attempt_at_unix_ms: 20,
                resume_reference: "resume.abcdef".into(),
            },
        ),
        Err(Error::InvalidStateTransition)
    ));

    for operation_id in [not_applied_id, retry_safe_id] {
        let state = store.mutation_state(operation_id).unwrap().unwrap();
        assert_eq!(state.phase, MutationPhase::Running);
        assert_eq!(state.state_version, 1);
        assert_eq!(store.mutation_events(operation_id).unwrap().len(), 2);
    }
}

#[test]
fn r3_typed_batch_registration_is_atomic_and_rejects_mismatched_intents() {
    let fixture = Fixture::new();
    let mut store = cursor_ready_store(&fixture);
    let operation_id = Uuid::new_v4();
    store
        .register_mutation_intent(&mutation_intent(operation_id, "typed-marker"), None)
        .unwrap();
    let batch_id = Uuid::new_v4();
    let mismatched = ChangeBatchDependency {
        operation_id,
        kind: ChangeBatchDependencyKind::MergePublication,
    };
    assert!(matches!(
        store.begin_r3_change_batch(batch_id, "cursor-1", "cursor-2", &[mismatched]),
        Err(Error::MutationCollision)
    ));
    assert!(store.active_change_batch().unwrap().is_none());
    assert_eq!(
        store
            .vault_state()
            .unwrap()
            .unwrap()
            .durable_cursor
            .as_deref(),
        Some("cursor-1")
    );

    let valid = ChangeBatchDependency {
        operation_id,
        kind: ChangeBatchDependencyKind::Mutation,
    };
    assert!(matches!(
        store.begin_r3_change_batch(batch_id, "cursor-1", "cursor-2", &[valid, valid]),
        Err(Error::MutationCollision)
    ));
    assert!(store.active_change_batch().unwrap().is_none());
}

#[test]
fn r3_typed_dependencies_require_exact_post_verify_evidence_before_cursor_commit() {
    let fixture = Fixture::new();
    let mut store = cursor_ready_store(&fixture);
    let operation_id = Uuid::new_v4();
    let dependency = ChangeBatchDependency {
        operation_id,
        kind: ChangeBatchDependencyKind::Mutation,
    };
    store
        .register_mutation_intent(&mutation_intent(operation_id, "preflight-marker"), None)
        .unwrap();
    let batch_id = Uuid::new_v4();
    store
        .begin_r3_change_batch(batch_id, "cursor-1", "cursor-2", &[dependency])
        .unwrap();
    assert!(matches!(
        store.begin_local_mutation(batch_id, &operation_id.to_string()),
        Err(Error::InvalidStateTransition)
    ));
    store.claim_mutation(operation_id, 0, 11).unwrap();
    let evidence_id = Uuid::new_v4();
    let mut preflight = mutation_evidence(
        evidence_id,
        operation_id,
        0,
        MutationDisposition::VerifiedApplied,
        Some("verified_applied"),
        "preflight-marker",
    );
    preflight.capture_phase = MutationEvidenceCapturePhase::Preflight;
    preflight.evidence_fingerprint = preflight.canonical_fingerprint();
    assert!(matches!(
        store.record_mutation_outcome(
            operation_id,
            1,
            &preflight,
            &MutationOutcomeTransition::VerifiedApplied,
        ),
        Err(Error::InvalidTransferEvidence)
    ));
    assert_eq!(
        store.mutation_state(operation_id).unwrap().unwrap().phase,
        MutationPhase::Running
    );
    assert!(matches!(
        store.commit_r3_change_dependency(batch_id, dependency, evidence_id),
        Err(Error::LocalMutationIncomplete)
    ));
    assert!(matches!(
        store.commit_r3_change_batch(batch_id, 21),
        Err(Error::LocalMutationIncomplete)
    ));
    assert_eq!(
        store
            .vault_state()
            .unwrap()
            .unwrap()
            .durable_cursor
            .as_deref(),
        Some("cursor-1")
    );
}

#[test]
fn r3_typed_batch_commits_mixed_dependencies_and_is_restart_safe() {
    let fixture = Fixture::new();
    let mut store = cursor_ready_store(&fixture);
    let dependencies = [
        (
            Uuid::new_v4(),
            ChangeBatchDependencyKind::Mutation,
            MutationOperationKind::LocalPublish,
        ),
        (
            Uuid::new_v4(),
            ChangeBatchDependencyKind::MergePublication,
            MutationOperationKind::MergePublish,
        ),
        (
            Uuid::new_v4(),
            ChangeBatchDependencyKind::ConflictCopyPublication,
            MutationOperationKind::ConflictCopyPublish,
        ),
        (
            Uuid::new_v4(),
            ChangeBatchDependencyKind::BasePublication,
            MutationOperationKind::BasePublish,
        ),
    ];
    let declared = dependencies
        .iter()
        .map(|(operation_id, kind, _)| ChangeBatchDependency {
            operation_id: *operation_id,
            kind: *kind,
        })
        .collect::<Vec<_>>();
    for (index, (operation_id, _, operation_kind)) in dependencies.iter().enumerate() {
        let marker = format!("typed-marker-{index}");
        let mut intent = mutation_intent(*operation_id, &marker);
        intent.operation_kind = *operation_kind;
        intent.intent_fingerprint = intent.canonical_fingerprint();
        store.register_mutation_intent(&intent, None).unwrap();
    }
    let batch_id = Uuid::new_v4();
    store
        .begin_r3_change_batch(batch_id, "cursor-1", "cursor-2", &declared)
        .unwrap();
    let evidence_ids = dependencies
        .iter()
        .enumerate()
        .map(|(index, (operation_id, _, _))| {
            complete_registered_r3_mutation(
                &mut store,
                *operation_id,
                &format!("typed-marker-{index}"),
            )
        })
        .collect::<Vec<_>>();
    for (dependency, evidence_id) in declared.iter().zip(evidence_ids) {
        store
            .commit_r3_change_dependency(batch_id, *dependency, evidence_id)
            .unwrap();
        store
            .commit_r3_change_dependency(batch_id, *dependency, evidence_id)
            .unwrap();
    }
    drop(store);
    let mut reopened = fixture.open();
    reopened.commit_r3_change_batch(batch_id, 21).unwrap();
    assert!(reopened.active_change_batch().unwrap().is_none());
    assert_eq!(
        reopened
            .vault_state()
            .unwrap()
            .unwrap()
            .durable_cursor
            .as_deref(),
        Some("cursor-2")
    );
}

#[test]
fn remote_existing_blocked_is_durable_needs_reconcile_without_running() {
    let fixture = Fixture::new();
    let operation_id = Uuid::new_v4();
    let mut intent = mutation_intent(operation_id, "blocked-marker");
    intent.operation_kind = MutationOperationKind::RemoteExistingBlocked;
    intent.account_id = Some("account-a".into());
    intent.remote_root_id = Some("remote-root".into());
    intent.remote_file_id = Some("remote-file".into());
    intent.source_parent_id = Some("remote-parent".into());
    intent.expected_remote_revision = Some("remote-revision".into());
    intent.intent_fingerprint = intent.canonical_fingerprint();
    let mut evidence = mutation_evidence(
        Uuid::new_v4(),
        operation_id,
        0,
        MutationDisposition::NeedsReconcile,
        Some("remote_existing_blocked"),
        "blocked-marker",
    );
    evidence.observed_account_id = intent.account_id.clone();
    evidence.observed_remote_root_id = intent.remote_root_id.clone();
    evidence.observed_remote_file_id = intent.remote_file_id.clone();
    evidence.observed_parent_id = intent.source_parent_id.clone();
    evidence.observed_path = intent.source_path.clone();
    evidence.observed_remote_revision = intent.expected_remote_revision.clone();
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    let mut store = fixture.open();
    assert_eq!(
        store
            .register_mutation_intent(&intent, Some(&evidence))
            .unwrap(),
        MutationRegistrationOutcome::Registered
    );
    let state = store.mutation_state(operation_id).unwrap().unwrap();
    assert_eq!(state.phase, MutationPhase::NeedsReconcile);
    assert_eq!(state.state_version, 1);
    assert_eq!(state.last_evidence_id, Some(evidence.evidence_id));
    assert_eq!(store.mutation_events(operation_id).unwrap().len(), 2);
    assert!(matches!(
        store.claim_mutation(operation_id, 1, 11),
        Err(Error::MutationNeedsReconcile)
    ));
}

#[test]
fn transfer_registration_is_exactly_idempotent_and_rejects_unsafe_references() {
    let fixture = Fixture::new();
    let mut store = bound_store(&fixture);
    let operation_id = Uuid::new_v4();
    let transfer = upload(operation_id, "operation-marker-a");
    assert_eq!(
        store.register_transfer(&transfer).unwrap(),
        TransferRegistrationOutcome::Registered
    );
    assert_eq!(
        store.register_transfer(&transfer).unwrap(),
        TransferRegistrationOutcome::AlreadyPresent
    );

    let mut conflicting = transfer.clone();
    conflicting.sha256 = hash(b'e');
    assert!(matches!(
        store.register_transfer(&conflicting),
        Err(Error::TransferCollision)
    ));
    assert!(matches!(
        store.register_transfer(&upload(Uuid::new_v4(), "operation-marker-a")),
        Err(Error::TransferCollision)
    ));

    assert!(TransferRecord::new(
        Uuid::new_v4(),
        TransferDirection::Upload,
        "hello.md",
        "remote-parent",
        None,
        Some(hash(b'a')),
        None,
        hash(b'b'),
        1,
        TransferMimeClass::Markdown,
        "marker-b",
        Some("/tmp/body".into()),
        None,
        1,
    )
    .is_err());
    assert!(TransferRecord::new(
        Uuid::new_v4(),
        TransferDirection::Download,
        "hello.md",
        "remote-parent",
        None,
        None,
        Some("remote-revision-1".into()),
        hash(b'b'),
        1,
        TransferMimeClass::Blob,
        "marker-c",
        None,
        None,
        1,
    )
    .is_err());
}

#[test]
fn offline_pause_preserves_attempt_count_and_resumes_only_when_due() {
    let fixture = Fixture::new();
    let mut store = bound_store(&fixture);
    let operation_id = Uuid::new_v4();
    store
        .register_transfer(&upload(operation_id, "marker-offline"))
        .unwrap();
    let claimed = store.claim_next_transfer(10).unwrap().unwrap();
    assert_eq!(claimed.attempt_count, 0);

    store
        .pause_transfer_offline(operation_id, 30, "network_offline", 20)
        .unwrap();
    let paused = store.transfer(operation_id).unwrap().unwrap();
    assert_eq!(paused.phase, TransferPhase::RetryScheduled);
    assert_eq!(paused.attempt_count, 0);
    let summary = store.transfer_summary().unwrap();
    assert_eq!(summary.retry_scheduled, 1);
    assert_eq!(summary.active(), 1);
    assert_eq!(summary.completed, 0);
    assert!(store.claim_next_transfer(29).unwrap().is_none());
    assert_eq!(
        store
            .claim_next_transfer(30)
            .unwrap()
            .unwrap()
            .attempt_count,
        0
    );
}

#[test]
fn fresh_authorization_reschedules_only_auth_paused_transfers() {
    let fixture = Fixture::new();
    let mut store = bound_store(&fixture);
    let auth_id = Uuid::new_v4();
    let pending_id = Uuid::new_v4();
    store
        .register_transfer(&upload(auth_id, "marker-auth-resume"))
        .unwrap();
    assert_eq!(
        store.claim_next_transfer(10).unwrap().unwrap().operation_id,
        auth_id
    );
    store
        .mark_transfer_auth_required(auth_id, "drive_unauthorized", 11)
        .unwrap();
    store
        .register_transfer(&upload(pending_id, "marker-pending-stays"))
        .unwrap();

    assert_eq!(store.resume_auth_required_transfers(20).unwrap(), 1);
    let resumed = store.transfer(auth_id).unwrap().unwrap();
    assert_eq!(resumed.phase, TransferPhase::RetryScheduled);
    assert_eq!(resumed.attempt_count, 1);
    assert_eq!(resumed.next_attempt_at_unix_ms, 20);
    assert_eq!(resumed.last_error_code.as_deref(), Some("auth_restored"));
    assert_eq!(
        store.transfer(pending_id).unwrap().unwrap().phase,
        TransferPhase::Pending
    );
    assert_eq!(store.resume_auth_required_transfers(20).unwrap(), 0);
}

#[test]
fn transfer_transitions_completion_and_base_publication_are_atomic_and_idempotent() {
    let fixture = Fixture::new();
    let mut store = bound_store(&fixture);
    let operation_id = Uuid::new_v4();
    let transfer = upload(operation_id, "operation-marker-a");
    store.register_transfer(&transfer).unwrap();

    assert_eq!(store.claim_next_transfer(9).unwrap(), None);
    assert_eq!(
        store.claim_next_transfer(10).unwrap().unwrap().phase,
        TransferPhase::Running
    );
    store
        .mark_transfer_auth_required(operation_id, "access_expired", 20)
        .unwrap();
    store
        .mark_transfer_auth_required(operation_id, "access_expired", 20)
        .unwrap();
    store
        .schedule_transfer_retry(operation_id, 30, "auth_restored", 21)
        .unwrap();
    store
        .schedule_transfer_retry(operation_id, 30, "auth_restored", 21)
        .unwrap();
    assert_eq!(
        store.transfer(operation_id).unwrap().unwrap().attempt_count,
        1
    );
    assert_eq!(
        store.claim_next_transfer(30).unwrap().unwrap().phase,
        TransferPhase::Running
    );
    store
        .mark_transfer_needs_reconcile(operation_id, "response_lost", 34)
        .unwrap();
    store
        .mark_transfer_needs_reconcile(operation_id, "response_lost", 34)
        .unwrap();

    store
        .publish_transfer_base_reference(operation_id, "base.abcdef", 35)
        .unwrap();
    store
        .publish_transfer_base_reference(operation_id, "base.abcdef", 35)
        .unwrap();
    assert!(matches!(
        store.publish_transfer_base_reference(operation_id, "base.different", 36),
        Err(Error::TransferCollision)
    ));

    let completion = completion();
    assert_eq!(
        store
            .complete_verified_transfer(operation_id, &completion)
            .unwrap(),
        TransferCompletionOutcome::Completed
    );
    assert_eq!(
        store
            .complete_verified_transfer(operation_id, &completion)
            .unwrap(),
        TransferCompletionOutcome::AlreadyCompleted
    );
    let completed = store.transfer(operation_id).unwrap().unwrap();
    assert_eq!(completed.phase, TransferPhase::Completed);
    assert_eq!(completed.remote_file_id.as_deref(), Some("remote-file"));
    assert_eq!(completed.base_reference.as_deref(), Some("base.abcdef"));
    assert_eq!(completed.last_error_code, None);
    assert_eq!(store.transfer_count().unwrap(), 0);
    assert_eq!(
        store.register_transfer(&transfer).unwrap(),
        TransferRegistrationOutcome::AlreadyCompleted
    );

    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    let history: i64 = connection
        .query_row("SELECT COUNT(*) FROM transfer_history", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(history, 1);
}

#[test]
fn completion_mismatch_rolls_back_history_and_preserves_running_evidence() {
    let fixture = Fixture::new();
    let mut store = bound_store(&fixture);
    let operation_id = Uuid::new_v4();
    store
        .register_transfer(&upload(operation_id, "operation-marker-a"))
        .unwrap();
    store.claim_next_transfer(10).unwrap().unwrap();
    let mut mismatched = completion();
    mismatched.local_revision = hash(b'c');
    assert!(matches!(
        store.complete_verified_transfer(operation_id, &mismatched),
        Err(Error::InvalidStateTransition)
    ));
    assert_eq!(
        store.transfer(operation_id).unwrap().unwrap().phase,
        TransferPhase::Running
    );
    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    let history: i64 = connection
        .query_row("SELECT COUNT(*) FROM transfer_history", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(history, 0);
}

#[test]
fn restart_converts_running_transfer_to_reconcile_without_blind_claim() {
    let fixture = Fixture::new();
    let operation_id = Uuid::new_v4();
    {
        let mut store = bound_store(&fixture);
        store
            .register_transfer(&upload(operation_id, "operation-marker-a"))
            .unwrap();
        store.claim_next_transfer(10).unwrap().unwrap();
    }

    let mut reopened = fixture.open();
    let recovered = reopened.transfer(operation_id).unwrap().unwrap();
    assert_eq!(recovered.phase, TransferPhase::NeedsReconcile);
    assert_eq!(
        recovered.last_error_code.as_deref(),
        Some("interrupted_unknown_outcome")
    );
    assert_eq!(reopened.claim_next_transfer(i64::MAX as u64).unwrap(), None);
    reopened
        .requeue_transfer_for_reconciliation(operation_id, 50)
        .unwrap();
    let queued = reopened.transfer(operation_id).unwrap().unwrap();
    assert_eq!(queued.phase, TransferPhase::RetryScheduled);
    assert_eq!(queued.attempt_count, 1);
    assert_eq!(queued.next_attempt_at_unix_ms, 50);
    assert_eq!(
        queued.last_error_code.as_deref(),
        Some("reconcile_requested")
    );
    assert_eq!(queued.remote_parent_id, "remote-parent");
    assert_eq!(queued.expected_local_revision, Some(hash(b'a')));
    assert_eq!(queued.sha256, hash(b'b'));
    assert_eq!(queued.stage_reference.as_deref(), Some("stage.abcdef"));
    assert!(queued.remote_file_id.is_none());
    assert!(queued.base_reference.is_none());
    assert!(reopened.claim_next_transfer(49).unwrap().is_none());
    let claimed = reopened.claim_next_transfer(50).unwrap().unwrap();
    assert_eq!(claimed.phase, TransferPhase::Running);
    assert_eq!(
        claimed.last_error_code.as_deref(),
        Some("reconcile_requested")
    );
}

#[test]
fn reconciliation_requeue_is_exact_single_phase_and_rejects_stale_time() {
    let fixture = Fixture::new();
    let mut store = bound_store(&fixture);
    let pending_id = Uuid::new_v4();
    store
        .register_transfer(&upload(pending_id, "marker-pending-not-reconcile"))
        .unwrap();
    assert!(matches!(
        store.requeue_transfer_for_reconciliation(pending_id, 20),
        Err(Error::InvalidStateTransition)
    ));
    assert_eq!(
        store.claim_next_transfer(10).unwrap().unwrap().operation_id,
        pending_id
    );
    assert!(matches!(
        store.requeue_transfer_for_reconciliation(pending_id, 20),
        Err(Error::InvalidStateTransition)
    ));

    let reconcile_id = Uuid::new_v4();
    store
        .register_transfer(&upload(reconcile_id, "marker-exact-reconcile"))
        .unwrap();
    store.claim_next_transfer(10).unwrap().unwrap();
    store
        .mark_transfer_needs_reconcile(reconcile_id, "response_lost", 30)
        .unwrap();
    assert!(matches!(
        store.requeue_transfer_for_reconciliation(reconcile_id, 29),
        Err(Error::InvalidStateTransition)
    ));
    assert_eq!(
        store.transfer(reconcile_id).unwrap().unwrap().phase,
        TransferPhase::NeedsReconcile
    );

    store
        .requeue_transfer_for_reconciliation(reconcile_id, 30)
        .unwrap();
    assert!(matches!(
        store.requeue_transfer_for_reconciliation(reconcile_id, 30),
        Err(Error::InvalidStateTransition)
    ));
    assert!(matches!(
        store.requeue_transfer_for_reconciliation(Uuid::new_v4(), 30),
        Err(Error::TransferNotFound)
    ));
}

#[test]
fn partial_v2_to_v3_migration_is_preserved_and_rejected() {
    let partial_migration = Fixture::new();
    let partial_migration_path;
    {
        let store = partial_migration.open();
        partial_migration_path = store.database_path().to_owned();
    }
    downgrade_to_v2(&partial_migration_path);
    let connection = rusqlite::Connection::open(&partial_migration_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE transfers (
                operation_id TEXT PRIMARY KEY NOT NULL,
                phase TEXT NOT NULL
             );",
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        SyncStore::open(
            &partial_migration.app_data,
            &partial_migration.vault,
            partial_migration.vault_id
        ),
        Err(Error::InvalidSchema)
    ));
    let connection = rusqlite::Connection::open(&partial_migration_path).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let partial_table: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'transfers'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(partial_table, 1);
}

#[test]
fn partial_and_constraint_weakened_v3_schemas_are_preserved_and_rejected() {
    let partial = Fixture::new();
    let partial_path;
    {
        let store = partial.open();
        partial_path = store.database_path().to_owned();
    }
    downgrade_to_v3(&partial_path);
    let connection = rusqlite::Connection::open(&partial_path).unwrap();
    connection
        .execute_batch("DROP TABLE transfer_history;")
        .unwrap();
    drop(connection);
    assert!(matches!(
        SyncStore::open(&partial.app_data, &partial.vault, partial.vault_id),
        Err(Error::InvalidSchema)
    ));
    let connection = rusqlite::Connection::open(&partial_path).unwrap();
    let missing: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'transfer_history'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(missing, 0);

    let weakened = Fixture::new();
    let weakened_path;
    {
        let store = weakened.open();
        weakened_path = store.database_path().to_owned();
    }
    downgrade_to_v3(&weakened_path);
    let connection = rusqlite::Connection::open(&weakened_path).unwrap();
    connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    connection
        .execute_batch(
            "ALTER TABLE transfers RENAME TO transfers_exact;
             CREATE TABLE transfers (
                operation_id TEXT PRIMARY KEY NOT NULL,
                direction TEXT NOT NULL,
                portable_path TEXT NOT NULL,
                remote_parent_id TEXT NOT NULL,
                remote_file_id TEXT,
                display_name TEXT NOT NULL,
                expected_local_revision TEXT,
                expected_remote_revision TEXT,
                sha256 TEXT NOT NULL,
                byte_length INTEGER NOT NULL,
                mime_class TEXT NOT NULL,
                operation_marker TEXT NOT NULL UNIQUE,
                stage_reference TEXT,
                base_reference TEXT,
                phase TEXT NOT NULL,
                attempt_count INTEGER NOT NULL,
                next_attempt_at_unix_ms INTEGER NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                last_error_code TEXT,
                verified_local_revision TEXT,
                verified_remote_revision TEXT
             );
             DROP INDEX transfers_due_idx;
             CREATE INDEX transfers_due_idx
                ON transfers(phase, next_attempt_at_unix_ms, created_at_unix_ms, operation_id);
             DROP TABLE transfers_exact;",
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        SyncStore::open(&weakened.app_data, &weakened.vault, weakened.vault_id),
        Err(Error::InvalidSchema)
    ));
}
