use myvault_sync_engine::{
    BindOutcome, ChangesPage, ScanPage, SyncStore, TransferCompletion, TransferDirection,
    TransferMimeClass, TransferPhase, TransferRecord, VerifiedRemoteBinding,
};
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

fn hash(byte: u8) -> String {
    std::iter::repeat_n(char::from(byte), 64).collect()
}

fn ready_store(fixture: &Fixture) -> SyncStore {
    let mut store = fixture.open();
    let binding =
        VerifiedRemoteBinding::new("account-a", "remote-root", "account-a", "remote-root")
            .expect("binding");
    assert_eq!(
        store.bind_remote_root(&binding, 1).expect("bind"),
        BindOutcome::Created
    );
    store
        .begin_initial_scan("start-token", 2)
        .expect("begin scan");
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

fn upload(operation_id: Uuid) -> TransferRecord {
    TransferRecord::new(
        operation_id,
        TransferDirection::Upload,
        "notes/fault-matrix.md",
        "remote-root",
        None,
        Some(hash(b'a')),
        None,
        hash(b'b'),
        42,
        TransferMimeClass::Markdown,
        format!("upload-{operation_id}"),
        Some(format!("stage-{operation_id}")),
        None,
        10,
    )
    .expect("upload record")
}

fn completion(occurred_at: u64) -> TransferCompletion {
    TransferCompletion::new(
        "remote-file",
        hash(b'c'),
        hash(b'a'),
        "base.abcdef",
        "uploaded_verified",
        occurred_at,
    )
    .expect("completion")
}

fn install_abort_trigger(database_path: &Path, name: &str, body: &str) {
    let connection = Connection::open(database_path).expect("fault connection");
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER {name} {body} BEGIN SELECT RAISE(ABORT, 'injected fault'); END;"
        ))
        .expect("install fault trigger");
}

#[derive(Clone, Copy, Debug)]
enum PersistenceBoundary {
    Enqueue,
    Claim,
    BaseReference,
    CompletionCommit,
    PreCursorAdvancement,
}

fn inject_enqueue_fault(store: &mut SyncStore, operation_id: Uuid, transfer: &TransferRecord) {
    install_abort_trigger(
        store.database_path(),
        "fault_enqueue",
        "BEFORE INSERT ON transfers",
    );
    assert!(store.register_transfer(transfer).is_err());
    assert!(store
        .transfer(operation_id)
        .expect("transfer lookup")
        .is_none());
}

fn inject_claim_fault(store: &mut SyncStore, operation_id: Uuid, transfer: &TransferRecord) {
    store.register_transfer(transfer).expect("register");
    install_abort_trigger(
        store.database_path(),
        "fault_claim",
        "BEFORE UPDATE OF phase ON transfers WHEN NEW.phase = 'running'",
    );
    assert!(store.claim_next_transfer(10).is_err());
    assert_eq!(
        store.transfer(operation_id).unwrap().unwrap().phase,
        TransferPhase::Pending
    );
}

fn inject_base_reference_fault(
    store: &mut SyncStore,
    operation_id: Uuid,
    transfer: &TransferRecord,
) {
    store.register_transfer(transfer).expect("register");
    store.claim_next_transfer(10).expect("claim").expect("due");
    install_abort_trigger(
        store.database_path(),
        "fault_base_reference",
        "BEFORE UPDATE OF base_reference ON transfers WHEN NEW.base_reference IS NOT NULL",
    );
    assert!(store
        .publish_transfer_base_reference(operation_id, "base.abcdef", 11)
        .is_err());
    let persisted = store.transfer(operation_id).unwrap().unwrap();
    assert_eq!(persisted.phase, TransferPhase::Running);
    assert!(persisted.base_reference.is_none());
}

fn inject_completion_fault(store: &mut SyncStore, operation_id: Uuid, transfer: &TransferRecord) {
    store.register_transfer(transfer).expect("register");
    store.claim_next_transfer(10).expect("claim").expect("due");
    install_abort_trigger(
        store.database_path(),
        "fault_completion_history",
        "BEFORE INSERT ON transfer_history",
    );
    assert!(store
        .complete_verified_transfer(operation_id, &completion(20))
        .is_err());
    let persisted = store.transfer(operation_id).unwrap().unwrap();
    assert_eq!(persisted.phase, TransferPhase::Running);
    assert!(persisted.base_reference.is_none());
    let connection = Connection::open(store.database_path()).unwrap();
    let history: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM transfer_history WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(history, 0);
}

fn inject_cursor_fault(store: &mut SyncStore) {
    let batch_id = Uuid::new_v4();
    store
        .begin_transfer_change_batch(batch_id, "cursor-1", "cursor-2", &[], &[])
        .expect("begin empty batch");
    install_abort_trigger(
        store.database_path(),
        "fault_cursor_commit",
        "BEFORE UPDATE OF durable_cursor ON vault_state WHEN NEW.durable_cursor = 'cursor-2'",
    );
    assert!(store.commit_transfer_change_batch(batch_id, 20).is_err());
    assert_eq!(
        store.active_change_batch().unwrap().unwrap().batch_id,
        batch_id
    );
}

#[test]
fn sqlite_fault_matrix_rolls_back_every_persistent_boundary() {
    for boundary in [
        PersistenceBoundary::Enqueue,
        PersistenceBoundary::Claim,
        PersistenceBoundary::BaseReference,
        PersistenceBoundary::CompletionCommit,
        PersistenceBoundary::PreCursorAdvancement,
    ] {
        let fixture = Fixture::new();
        let mut store = ready_store(&fixture);
        let operation_id = Uuid::new_v4();
        let transfer = upload(operation_id);

        match boundary {
            PersistenceBoundary::Enqueue => {
                inject_enqueue_fault(&mut store, operation_id, &transfer);
            }
            PersistenceBoundary::Claim => {
                inject_claim_fault(&mut store, operation_id, &transfer);
            }
            PersistenceBoundary::BaseReference => {
                inject_base_reference_fault(&mut store, operation_id, &transfer);
            }
            PersistenceBoundary::CompletionCommit => {
                inject_completion_fault(&mut store, operation_id, &transfer);
            }
            PersistenceBoundary::PreCursorAdvancement => inject_cursor_fault(&mut store),
        }

        assert_eq!(
            store
                .vault_state()
                .unwrap()
                .unwrap()
                .durable_cursor
                .as_deref(),
            Some("cursor-1"),
            "boundary: {boundary:?}"
        );
    }
}

#[test]
fn pre_cursor_fault_preserves_committed_local_evidence_without_advancing_cursor() {
    let fixture = Fixture::new();
    let mut store = ready_store(&fixture);
    let batch_id = Uuid::new_v4();
    store
        .begin_change_batch(batch_id, "cursor-1", "cursor-2", ["publish-local"])
        .expect("begin batch");
    store
        .begin_local_mutation(batch_id, "publish-local")
        .expect("begin local mutation");
    store
        .mark_local_mutation_committed(batch_id, "publish-local")
        .expect("commit local mutation");
    install_abort_trigger(
        store.database_path(),
        "fault_cursor_after_local_commit",
        "BEFORE UPDATE OF durable_cursor ON vault_state WHEN NEW.durable_cursor = 'cursor-2'",
    );
    assert!(store.commit_change_batch(batch_id, 20).is_err());
    assert_eq!(
        store
            .vault_state()
            .unwrap()
            .unwrap()
            .durable_cursor
            .as_deref(),
        Some("cursor-1")
    );
    assert_eq!(
        store.local_mutations(batch_id).unwrap()[0].state,
        myvault_sync_engine::LocalMutationState::Committed
    );
}
