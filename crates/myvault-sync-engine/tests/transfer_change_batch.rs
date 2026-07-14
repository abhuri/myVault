use myvault_sync_engine::{
    BindOutcome, ChangesPage, Error, LocalMutationState, RemoteChange, RemoteContentHash,
    RemoteEntry, RemoteEntryKind, RemoteHashAlgorithm, ScanPage, SyncStore, TransferCompletion,
    TransferCompletionOutcome, TransferDirection, TransferMimeClass, TransferPhase, TransferRecord,
    VerifiedRemoteBinding,
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

fn hash(byte: u8) -> String {
    std::iter::repeat_n(char::from(byte), 64).collect()
}

fn remote_file(path: &str, revision: &str, digest: &str) -> RemoteEntry {
    RemoteEntry {
        file_id: "remote-file".into(),
        parent_id: "remote-root".into(),
        path: path.into(),
        kind: RemoteEntryKind::File,
        content_hash: Some(
            RemoteContentHash::new(RemoteHashAlgorithm::Sha256, digest).expect("hash"),
        ),
        remote_revision: revision.into(),
    }
}

fn download(operation_id: Uuid, entry: &RemoteEntry) -> TransferRecord {
    TransferRecord::new(
        operation_id,
        TransferDirection::Download,
        entry.path.clone(),
        entry.parent_id.clone(),
        Some(entry.file_id.clone()),
        None,
        Some(entry.remote_revision.clone()),
        entry.content_hash.as_ref().expect("file hash").hex.clone(),
        42,
        TransferMimeClass::Markdown,
        format!("download-{operation_id}"),
        Some(format!("stage.{operation_id}")),
        None,
        10,
    )
    .expect("download")
}

fn completion(entry: &RemoteEntry, occurred_at: u64) -> TransferCompletion {
    TransferCompletion::new(
        entry.file_id.clone(),
        entry.remote_revision.clone(),
        hash(b'c'),
        "base.abcdef",
        "downloaded_verified",
        occurred_at,
    )
    .expect("completion")
}

fn ready_store(fixture: &Fixture, initial: &[RemoteEntry]) -> SyncStore {
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
        .expect("initial scan");
    store
        .apply_scan_page(
            None,
            &ScanPage {
                entries: initial.to_vec(),
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

#[test]
fn transfer_batch_survives_restart_and_advances_only_after_verified_completion() {
    let fixture = Fixture::new();
    let mut store = ready_store(&fixture, &[]);
    let batch_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let entry = remote_file("notes/hello.md", "remote-revision-2", &hash(b'a'));
    let transfer = download(operation_id, &entry);

    store
        .begin_transfer_change_batch(
            batch_id,
            "cursor-1",
            "cursor-2",
            &[RemoteChange::Upsert(entry.clone())],
            std::slice::from_ref(&transfer),
        )
        .expect("begin transfer batch");
    assert_eq!(
        store.remote_entry("remote-file").unwrap(),
        Some(entry.clone())
    );
    assert_eq!(
        store.transfer(operation_id).unwrap().unwrap().phase,
        TransferPhase::Pending
    );
    assert_eq!(
        store.local_mutations(batch_id).unwrap()[0].state,
        LocalMutationState::Pending
    );
    assert!(matches!(
        store.commit_transfer_change_batch(batch_id, 11),
        Err(Error::LocalMutationIncomplete)
    ));
    assert!(matches!(
        store.commit_change_batch(batch_id, 11),
        Err(Error::InvalidStateTransition)
    ));
    assert!(matches!(
        store.abort_change_batch(batch_id),
        Err(Error::MutationNeedsReconcile)
    ));

    store.claim_next_transfer(10).unwrap().unwrap();
    assert!(matches!(
        store.complete_verified_transfer(operation_id, &completion(&entry, 20)),
        Err(Error::InvalidStateTransition)
    ));
    assert_eq!(
        store.transfer(operation_id).unwrap().unwrap().phase,
        TransferPhase::Running
    );
    store
        .begin_transfer_local_publish(operation_id, 11)
        .expect("begin local publish");
    drop(store);

    let mut reopened = fixture.open();
    assert_eq!(
        reopened.transfer(operation_id).unwrap().unwrap().phase,
        TransferPhase::NeedsReconcile
    );
    assert_eq!(
        reopened.local_mutations(batch_id).unwrap()[0].state,
        LocalMutationState::Applying
    );
    reopened
        .requeue_transfer_for_reconciliation(operation_id, 11)
        .expect("request exact reconciliation");
    reopened
        .claim_next_transfer(11)
        .expect("claim reconciliation")
        .expect("due reconciliation");
    reopened
        .begin_transfer_local_publish(operation_id, 11)
        .expect("explicit reconciliation may repeat create-no-replace publication");
    assert_eq!(
        reopened
            .complete_verified_transfer(operation_id, &completion(&entry, 20))
            .unwrap(),
        TransferCompletionOutcome::Completed
    );
    assert_eq!(
        reopened.local_mutations(batch_id).unwrap()[0].state,
        LocalMutationState::Committed
    );

    let connection = rusqlite::Connection::open(reopened.database_path()).unwrap();
    let base: (String, String, String) = connection
        .query_row(
            "SELECT base_local_revision, base_remote_revision, base_content_hash
             FROM remote_entries WHERE file_id = 'remote-file'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        base,
        (hash(b'c'), entry.remote_revision.clone(), hash(b'a'))
    );
    reopened
        .commit_transfer_change_batch(batch_id, 21)
        .expect("commit cursor");
    assert_eq!(
        reopened
            .vault_state()
            .unwrap()
            .unwrap()
            .durable_cursor
            .as_deref(),
        Some("cursor-2")
    );
    assert!(reopened.active_change_batch().unwrap().is_none());
}

#[test]
fn rejected_transfer_batch_is_fully_atomic() {
    let fixture = Fixture::new();
    let mut store = ready_store(&fixture, &[]);
    let batch_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let entry = remote_file("hello.md", "remote-revision-2", &hash(b'a'));
    let mut mismatched = download(operation_id, &entry);
    mismatched.sha256 = hash(b'b');

    assert!(matches!(
        store.begin_transfer_change_batch(
            batch_id,
            "cursor-1",
            "cursor-2",
            &[RemoteChange::Upsert(entry)],
            &[mismatched]
        ),
        Err(Error::TransferChangeMismatch)
    ));
    assert!(store.remote_entry("remote-file").unwrap().is_none());
    assert!(store.transfer(operation_id).unwrap().is_none());
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
}

#[test]
fn removals_moves_kind_changes_and_unresolved_file_revisions_fail_closed() {
    let fixture = Fixture::new();
    let initial = remote_file("hello.md", "remote-revision-1", &hash(b'a'));
    let mut store = ready_store(&fixture, std::slice::from_ref(&initial));

    assert!(matches!(
        store.begin_transfer_change_batch(
            Uuid::new_v4(),
            "cursor-1",
            "cursor-2",
            &[RemoteChange::Removed {
                file_id: initial.file_id.clone()
            }],
            &[]
        ),
        Err(Error::UnsupportedTransferChange)
    ));

    let mut moved = initial.clone();
    moved.path = "moved.md".into();
    moved.remote_revision = "remote-revision-2".into();
    let moved_transfer = download(Uuid::new_v4(), &moved);
    assert!(matches!(
        store.begin_transfer_change_batch(
            Uuid::new_v4(),
            "cursor-1",
            "cursor-2",
            &[RemoteChange::Upsert(moved)],
            &[moved_transfer]
        ),
        Err(Error::UnsupportedTransferChange)
    ));

    let mut changed = initial.clone();
    changed.remote_revision = "remote-revision-2".into();
    changed.content_hash =
        Some(RemoteContentHash::new(RemoteHashAlgorithm::Sha256, hash(b'b')).expect("new hash"));
    assert!(matches!(
        store.begin_transfer_change_batch(
            Uuid::new_v4(),
            "cursor-1",
            "cursor-2",
            &[RemoteChange::Upsert(changed)],
            &[]
        ),
        Err(Error::TransferChangeMismatch)
    ));
    assert_eq!(store.remote_entry("remote-file").unwrap(), Some(initial));
    assert!(store.active_change_batch().unwrap().is_none());
}

#[test]
fn zero_mutation_and_unchanged_metadata_pages_can_advance() {
    let fixture = Fixture::new();
    let initial = remote_file("hello.md", "remote-revision-1", &hash(b'a'));
    let mut store = ready_store(&fixture, std::slice::from_ref(&initial));
    let empty_batch = Uuid::new_v4();
    store
        .begin_transfer_change_batch(empty_batch, "cursor-1", "cursor-2", &[], &[])
        .unwrap();
    store.commit_transfer_change_batch(empty_batch, 10).unwrap();

    let unchanged_batch = Uuid::new_v4();
    store
        .begin_transfer_change_batch(
            unchanged_batch,
            "cursor-2",
            "cursor-3",
            &[RemoteChange::Upsert(initial)],
            &[],
        )
        .unwrap();
    assert_eq!(
        store
            .active_change_batch()
            .unwrap()
            .unwrap()
            .declared_mutations,
        0
    );
    store
        .commit_transfer_change_batch(unchanged_batch, 11)
        .unwrap();
    assert_eq!(
        store
            .vault_state()
            .unwrap()
            .unwrap()
            .durable_cursor
            .as_deref(),
        Some("cursor-3")
    );
}

#[test]
fn completion_and_cursor_commit_reject_tampered_metadata_without_partial_writes() {
    let fixture = Fixture::new();
    let mut store = ready_store(&fixture, &[]);
    let batch_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let entry = remote_file("hello.md", "remote-revision-2", &hash(b'a'));
    let transfer = download(operation_id, &entry);
    store
        .begin_transfer_change_batch(
            batch_id,
            "cursor-1",
            "cursor-2",
            &[RemoteChange::Upsert(entry.clone())],
            &[transfer],
        )
        .unwrap();
    store.claim_next_transfer(10).unwrap().unwrap();
    store
        .begin_transfer_local_publish(operation_id, 11)
        .unwrap();

    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    connection
        .execute(
            "UPDATE remote_entries SET remote_revision = 'tampered-revision'
             WHERE file_id = 'remote-file'",
            [],
        )
        .unwrap();
    assert!(matches!(
        store.complete_verified_transfer(operation_id, &completion(&entry, 20)),
        Err(Error::TransferChangeMismatch)
    ));
    assert_eq!(
        store.transfer(operation_id).unwrap().unwrap().phase,
        TransferPhase::Running
    );
    assert_eq!(
        store.local_mutations(batch_id).unwrap()[0].state,
        LocalMutationState::Applying
    );
    let history: i64 = connection
        .query_row("SELECT COUNT(*) FROM transfer_history", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(history, 0);

    connection
        .execute(
            "UPDATE remote_entries SET remote_revision = ?1 WHERE file_id = 'remote-file'",
            [&entry.remote_revision],
        )
        .unwrap();
    store
        .complete_verified_transfer(operation_id, &completion(&entry, 20))
        .unwrap();
    connection
        .execute(
            "UPDATE remote_entries SET base_content_hash = ?1 WHERE file_id = 'remote-file'",
            [hash(b'f')],
        )
        .unwrap();
    assert!(matches!(
        store.commit_transfer_change_batch(batch_id, 21),
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
    connection
        .execute(
            "UPDATE remote_entries SET base_content_hash = ?1 WHERE file_id = 'remote-file'",
            [hash(b'a')],
        )
        .unwrap();
    store.commit_transfer_change_batch(batch_id, 22).unwrap();
}
