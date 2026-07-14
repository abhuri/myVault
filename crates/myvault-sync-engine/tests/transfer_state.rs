use myvault_sync_engine::{
    BindOutcome, EnqueueOutcome, Error, JobState, QueueJob, QueueJobKind, RemoteContentHash,
    RemoteEntry, RemoteEntryKind, RemoteHashAlgorithm, ScanPage, SyncStore, TransferCompletion,
    TransferCompletionOutcome, TransferDirection, TransferMimeClass, TransferPhase, TransferRecord,
    TransferRegistrationOutcome, VerifiedRemoteBinding, SCHEMA_VERSION,
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

fn hash(byte: u8) -> String {
    std::iter::repeat_n(char::from(byte), 64).collect()
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
            "DROP TABLE transfer_history;
             DROP INDEX transfers_due_idx;
             DROP TABLE transfers;
             PRAGMA user_version = 2;",
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
        .schedule_transfer_retry(operation_id, 50, "verified_absent", 40)
        .unwrap();
    assert_eq!(
        reopened.claim_next_transfer(50).unwrap().unwrap().phase,
        TransferPhase::Running
    );
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
