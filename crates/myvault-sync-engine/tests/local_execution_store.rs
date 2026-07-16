use myvault_sync_engine::{Error, SyncStore, SCHEMA_VERSION};
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use uuid::Uuid;

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
}

#[cfg(unix)]
fn make_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private mode");
}

#[cfg(not(unix))]
fn make_private(_path: &Path) {}

#[test]
fn fresh_v6_has_empty_local_execution_ledger_and_rejects_partial_contract() {
    let fixture = Fixture::new();
    let store = fixture.open();
    assert_eq!(
        store.schema_version().expect("schema version"),
        SCHEMA_VERSION
    );
    let database_path = store.database_path().to_owned();
    drop(store);

    let connection = Connection::open(database_path).expect("database");
    connection
        .pragma_update(None, "foreign_keys", true)
        .expect("foreign keys");
    for table in [
        "local_execution_contracts",
        "local_execution_identity_evidence",
        "local_execution_collision_members",
        "local_execution_contract_completions",
        "local_execution_attempt_boundaries",
        "local_execution_attempt_outcomes",
        "local_execution_r3_bridge_receipts",
        "local_execution_r3_consumption_anchors",
        "mutation_retry_contracts",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("empty ledger");
        assert_eq!(count, 0, "{table}");
    }
    let operation_id = Uuid::new_v4().to_string();
    let result = connection.execute(
        "INSERT INTO local_execution_contracts(
            operation_id, vault_id, intent_fingerprint, contract_fingerprint,
            target_name, target_collision_key, collision_member_count,
            collision_snapshot_fingerprint, completion_id, registered_at_unix_ms
         ) VALUES (?1, ?2, zeroblob(32), zeroblob(32), 'name', 'name', 0, zeroblob(32), 'missing-completion', 1)",
        [&operation_id, &fixture.vault_id.to_string()],
    );
    assert!(result.is_err(), "a partial contract must not commit");
}

#[test]
fn partial_and_trigger_weakened_v6_are_preserved_and_rejected() {
    for tamper in [
        "DROP TRIGGER local_execution_completion_validate;",
        "DROP TRIGGER local_execution_outcomes_no_update;
         CREATE TRIGGER local_execution_outcomes_no_update
         BEFORE UPDATE ON local_execution_attempt_outcomes BEGIN SELECT 1; END;",
    ] {
        let fixture = Fixture::new();
        let store = fixture.open();
        let database_path = store.database_path().to_owned();
        drop(store);
        let connection = Connection::open(&database_path).expect("database");
        connection.execute_batch(tamper).expect("tamper v6 schema");
        drop(connection);

        assert!(matches!(
            SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
            Err(Error::InvalidSchema)
        ));
        let preserved = Connection::open(&database_path).expect("preserved database");
        let version: i64 = preserved
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("preserved version");
        assert_eq!(version, SCHEMA_VERSION);
    }
}

#[test]
fn v6_global_reverse_scans_reject_orphan_anchor_and_retry_contract() {
    for table in [
        "local_execution_r3_consumption_anchors",
        "mutation_retry_contracts",
    ] {
        let fixture = Fixture::new();
        let store = fixture.open();
        let database_path = store.database_path().to_owned();
        drop(store);
        let connection = Connection::open(&database_path).expect("database");
        let operation = Uuid::new_v4().to_string();
        if table == "local_execution_r3_consumption_anchors" {
            connection
                .execute(
                    "INSERT INTO local_execution_r3_consumption_anchors(
                    anchor_id, anchor_fingerprint, receipt_id, receipt_fingerprint,
                    operation_id, attempt_number, outcome_id, evidence_id,
                    r3_evidence_fingerprint, dependency_kind
                 ) VALUES (?1, zeroblob(32), ?2, zeroblob(32), ?3, 0, ?4, ?5, ?6, 'mutation')",
                    [
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        operation,
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        "a".repeat(64),
                    ],
                )
                .expect("insert orphan anchor");
        } else {
            connection
                .execute(
                    "INSERT INTO mutation_retry_contracts(
                    operation_id, state_version, attempt_number, evidence_id, evidence_fingerprint,
                    disposition, outcome_code, due_at_unix_ms, retry_mode, resume_reference,
                    verified_received_byte_offset, captured_at_unix_ms
                 ) VALUES (?1, 1, 0, ?2, ?3, 'retry_safe', 'retry_safe', 20,
                           'resume_exact', 'resume.ref', 0, 10)",
                    [operation, Uuid::new_v4().to_string(), "b".repeat(64)],
                )
                .expect("insert orphan retry contract");
        }
        drop(connection);
        assert!(
            matches!(
                SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
                Err(Error::InvalidSchema)
            ),
            "{table} orphan must fail global reopen"
        );
    }
}
