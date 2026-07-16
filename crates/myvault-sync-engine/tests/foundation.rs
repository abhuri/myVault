use myvault_sync_engine::{
    advance_initial_sync, BindOutcome, ChangesPage, DriveClient, EnqueueOutcome, Error,
    InitialSyncProgress, JobState, LocalMutationState, QueueJob, QueueJobKind, RemoteChange,
    RemoteContentHash, RemoteEntry, RemoteEntryKind, RemoteError, RemoteHashAlgorithm,
    RemotePreviewCursor, ScanPage, ScanRequest, SyncPhase, SyncStore, VerifiedRemoteBinding,
    SCHEMA_VERSION,
};
use std::collections::VecDeque;
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

#[derive(Default)]
struct MockDrive {
    start_token: Option<String>,
    scans: VecDeque<(Option<String>, ScanPage)>,
    changes: VecDeque<(String, ChangesPage)>,
    calls: Vec<String>,
}

impl DriveClient for MockDrive {
    fn get_start_page_token(&mut self) -> Result<String, RemoteError> {
        self.calls.push("start".into());
        self.start_token
            .take()
            .ok_or_else(|| remote_error("missing_start"))
    }

    fn scan_folder_page(&mut self, request: &ScanRequest) -> Result<ScanPage, RemoteError> {
        self.calls.push(format!(
            "scan:{}:{}",
            request.folder_id,
            request.page_token.as_deref().unwrap_or("first")
        ));
        let (expected, page) = self
            .scans
            .pop_front()
            .ok_or_else(|| remote_error("missing_scan_page"))?;
        if expected != request.page_token {
            return Err(remote_error("unexpected_scan_cursor"));
        }
        Ok(page)
    }

    fn changes_page(&mut self, page_token: &str) -> Result<ChangesPage, RemoteError> {
        self.calls.push(format!("changes:{page_token}"));
        let (expected, page) = self
            .changes
            .pop_front()
            .ok_or_else(|| remote_error("missing_changes_page"))?;
        if expected != page_token {
            return Err(remote_error("unexpected_changes_cursor"));
        }
        Ok(page)
    }
}

struct ExpiredCursorDrive;

impl DriveClient for ExpiredCursorDrive {
    fn get_start_page_token(&mut self) -> Result<String, RemoteError> {
        unreachable!()
    }

    fn scan_folder_page(&mut self, _: &ScanRequest) -> Result<ScanPage, RemoteError> {
        unreachable!()
    }

    fn changes_page(&mut self, _: &str) -> Result<ChangesPage, RemoteError> {
        Err(remote_error("cursor_expired"))
    }
}

fn binding(account_id: &str, root_id: &str) -> VerifiedRemoteBinding {
    VerifiedRemoteBinding::new(account_id, root_id, account_id, root_id).unwrap()
}

fn downgrade_database_to_v1(database_path: &Path) {
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
             DROP TABLE scan_frontier;
             DROP INDEX remote_entries_preview_idx;
             ALTER TABLE vault_state RENAME TO vault_state_v2;
             CREATE TABLE vault_state (
                singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
                vault_id TEXT NOT NULL UNIQUE,
                remote_root_id TEXT NOT NULL,
                phase TEXT NOT NULL CHECK (phase IN ('need_start_token', 'scanning', 'draining', 'ready')),
                start_token TEXT,
                scan_page_token TEXT,
                changes_page_token TEXT,
                durable_cursor TEXT,
                updated_at_unix_ms INTEGER NOT NULL CHECK (updated_at_unix_ms >= 0)
             );
             INSERT INTO vault_state(
                singleton, vault_id, remote_root_id, phase, start_token,
                scan_page_token, changes_page_token, durable_cursor, updated_at_unix_ms
             )
             SELECT singleton, vault_id, remote_root_id, phase, start_token,
                    scan_page_token, changes_page_token, durable_cursor, updated_at_unix_ms
             FROM vault_state_v2;
             DROP TABLE vault_state_v2;
             PRAGMA user_version = 1;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
}

fn remote_error(code: &str) -> RemoteError {
    RemoteError::new(code).expect("remote error code")
}

fn hash(byte: u8) -> String {
    std::iter::repeat_n(char::from(byte), 64).collect()
}

fn file(file_id: &str, path: &str, revision: &str, hash_byte: u8) -> RemoteEntry {
    RemoteEntry {
        file_id: file_id.into(),
        parent_id: "remote-root".into(),
        path: path.into(),
        kind: RemoteEntryKind::File,
        content_hash: Some(
            RemoteContentHash::new(RemoteHashAlgorithm::Sha256, hash(hash_byte)).unwrap(),
        ),
        remote_revision: revision.into(),
    }
}

fn folder(file_id: &str, parent_id: &str, path: &str, revision: &str) -> RemoteEntry {
    RemoteEntry {
        file_id: file_id.into(),
        parent_id: parent_id.into(),
        path: path.into(),
        kind: RemoteEntryKind::Folder,
        content_hash: None,
        remote_revision: revision.into(),
    }
}

fn child_file(
    file_id: &str,
    parent_id: &str,
    path: &str,
    revision: &str,
    hash_byte: u8,
) -> RemoteEntry {
    let mut entry = file(file_id, path, revision, hash_byte);
    entry.parent_id = parent_id.into();
    entry
}

fn ready_store(fixture: &Fixture) -> SyncStore {
    let mut store = fixture.open();
    assert_eq!(
        store
            .bind_remote_root(&binding("account-a", "remote-root"), 1)
            .unwrap(),
        BindOutcome::Created
    );
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
            &ChangesPage {
                changes: Vec::new(),
                next_page_token: None,
                new_start_page_token: Some("durable-1".into()),
            },
            4,
        )
        .unwrap();
    store
}

#[test]
fn initial_sync_orders_token_scan_and_changes_then_reaches_ready() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .bind_remote_root(&binding("account-a", "remote-root"), 1)
        .unwrap();
    let mut drive = MockDrive {
        start_token: Some("start-1".into()),
        scans: VecDeque::from([
            (
                None,
                ScanPage {
                    entries: vec![
                        file("file-a", "A.md", "rev-a1", b'a'),
                        file("file-b", "ไทย.md", "rev-b1", b'b'),
                    ],
                    next_page_token: Some("scan-2".into()),
                },
            ),
            (
                Some("scan-2".into()),
                ScanPage {
                    entries: vec![file("file-c", "image.png", "rev-c1", b'c')],
                    next_page_token: None,
                },
            ),
        ]),
        changes: VecDeque::from([
            (
                "start-1".into(),
                ChangesPage {
                    changes: vec![RemoteChange::Removed {
                        file_id: "file-a".into(),
                    }],
                    next_page_token: Some("changes-2".into()),
                    new_start_page_token: None,
                },
            ),
            (
                "changes-2".into(),
                ChangesPage {
                    changes: vec![RemoteChange::Upsert(file(
                        "file-b",
                        "ไทย.md",
                        "rev-b2",
                        b'd',
                    ))],
                    next_page_token: None,
                    new_start_page_token: Some("durable-2".into()),
                },
            ),
        ]),
        calls: Vec::new(),
    };

    assert_eq!(
        advance_initial_sync(&mut store, &mut drive, 2).unwrap(),
        InitialSyncProgress::StartTokenCaptured
    );
    assert_eq!(
        advance_initial_sync(&mut store, &mut drive, 3).unwrap(),
        InitialSyncProgress::ScanPageCommitted
    );
    assert_eq!(
        advance_initial_sync(&mut store, &mut drive, 4).unwrap(),
        InitialSyncProgress::ScanComplete
    );
    assert_eq!(
        advance_initial_sync(&mut store, &mut drive, 5).unwrap(),
        InitialSyncProgress::ChangesPageCommitted
    );
    assert_eq!(
        advance_initial_sync(&mut store, &mut drive, 6).unwrap(),
        InitialSyncProgress::Ready
    );

    let state = store.vault_state().unwrap().unwrap();
    assert_eq!(state.phase, SyncPhase::Ready);
    assert_eq!(state.durable_cursor.as_deref(), Some("durable-2"));
    assert_eq!(store.remote_entry_count().unwrap(), 2);
    assert_eq!(
        drive.calls,
        [
            "start",
            "scan:remote-root:first",
            "scan:remote-root:scan-2",
            "changes:start-1",
            "changes:changes-2",
        ]
    );
}

#[test]
fn token_capture_and_recursive_frontier_are_restart_safe() {
    let fixture = Fixture::new();
    {
        let mut store = fixture.open();
        store
            .bind_remote_root(&binding("account-a", "remote-root"), 1)
            .unwrap();
        store.begin_initial_scan("start-1", 2).unwrap();
        let request = store.scan_request().unwrap().unwrap();
        assert_eq!(request.folder_id, "remote-root");
        assert_eq!(request.folder_path, "");
        assert_eq!(request.page_token, None);
    }

    let mut store = fixture.open();
    let state = store.vault_state().unwrap().unwrap();
    assert_eq!(state.phase, SyncPhase::Scanning);
    assert_eq!(state.start_token.as_deref(), Some("start-1"));
    store
        .apply_scan_page(
            None,
            &ScanPage {
                entries: vec![folder("folder-a", "remote-root", "Notes", "folder-rev")],
                next_page_token: Some("root-page-2".into()),
            },
            3,
        )
        .unwrap();
    assert_eq!(
        store.scan_request().unwrap().unwrap().page_token.as_deref(),
        Some("root-page-2")
    );
    store
        .apply_scan_page(
            Some("root-page-2"),
            &ScanPage {
                entries: Vec::new(),
                next_page_token: None,
            },
            4,
        )
        .unwrap();
    let child = store.scan_request().unwrap().unwrap();
    assert_eq!(child.folder_id, "folder-a");
    assert_eq!(child.folder_path, "Notes");
    store
        .apply_scan_page(
            None,
            &ScanPage {
                entries: vec![child_file(
                    "nested-note",
                    "folder-a",
                    "Notes/A.md",
                    "note-rev",
                    b'a',
                )],
                next_page_token: Some("folder-page-2".into()),
            },
            5,
        )
        .unwrap();
    drop(store);

    let store = fixture.open();
    assert_eq!(
        store.scan_request().unwrap().unwrap(),
        ScanRequest {
            folder_id: "folder-a".into(),
            folder_path: "Notes".into(),
            page_token: Some("folder-page-2".into()),
        }
    );
    assert_eq!(store.remote_entry_count().unwrap(), 2);
}

#[test]
fn recursive_frontier_rejects_root_and_seen_folder_cycles_atomically() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .bind_remote_root(&binding("account-a", "remote-root"), 1)
        .unwrap();
    store.begin_initial_scan("start-1", 2).unwrap();

    assert!(matches!(
        store.apply_scan_page(
            None,
            &ScanPage {
                entries: vec![folder("remote-root", "remote-root", "Loop", "rev-loop")],
                next_page_token: None,
            },
            3,
        ),
        Err(Error::InvalidRemoteEntry)
    ));
    assert_eq!(store.remote_entry_count().unwrap(), 0);
    assert_eq!(
        store.scan_request().unwrap().unwrap().folder_id,
        "remote-root"
    );

    store
        .apply_scan_page(
            None,
            &ScanPage {
                entries: vec![folder("folder-a", "remote-root", "Notes", "rev-a")],
                next_page_token: None,
            },
            4,
        )
        .unwrap();
    assert_eq!(store.scan_request().unwrap().unwrap().folder_id, "folder-a");
    store
        .apply_scan_page(
            None,
            &ScanPage {
                entries: vec![folder("folder-b", "folder-a", "Notes/Child", "rev-b")],
                next_page_token: None,
            },
            5,
        )
        .unwrap();
    assert_eq!(store.scan_request().unwrap().unwrap().folder_id, "folder-b");
    assert!(matches!(
        store.apply_scan_page(
            None,
            &ScanPage {
                entries: vec![folder(
                    "folder-a",
                    "folder-b",
                    "Notes/Child/Loop",
                    "rev-loop",
                )],
                next_page_token: None,
            },
            6,
        ),
        Err(Error::InvalidRemoteEntry)
    ));
    assert_eq!(store.remote_entry_count().unwrap(), 2);
    assert_eq!(store.scan_request().unwrap().unwrap().folder_id, "folder-b");
}

#[test]
fn restart_after_scan_before_changes_resumes_drain() {
    let fixture = Fixture::new();
    {
        let mut store = fixture.open();
        store
            .bind_remote_root(&binding("account-a", "remote-root"), 1)
            .unwrap();
        store.begin_initial_scan("start-1", 2).unwrap();
        store
            .apply_scan_page(
                None,
                &ScanPage {
                    entries: vec![file("file-a", "A.md", "rev-a", b'a')],
                    next_page_token: None,
                },
                3,
            )
            .unwrap();
        assert_eq!(
            store.vault_state().unwrap().unwrap().phase,
            SyncPhase::Draining
        );
    }

    let mut store = fixture.open();
    let mut drive = MockDrive {
        changes: VecDeque::from([(
            "start-1".into(),
            ChangesPage {
                changes: Vec::new(),
                next_page_token: None,
                new_start_page_token: Some("durable-1".into()),
            },
        )]),
        ..MockDrive::default()
    };
    assert_eq!(
        advance_initial_sync(&mut store, &mut drive, 4).unwrap(),
        InitialSyncProgress::Ready
    );
    assert_eq!(
        store
            .vault_state()
            .unwrap()
            .unwrap()
            .durable_cursor
            .as_deref(),
        Some("durable-1")
    );
}

#[test]
fn mid_changes_restart_and_rejected_page_do_not_advance_cursor() {
    let fixture = Fixture::new();
    {
        let mut store = fixture.open();
        store
            .bind_remote_root(&binding("account-a", "remote-root"), 1)
            .unwrap();
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
                &ChangesPage {
                    changes: Vec::new(),
                    next_page_token: Some("changes-2".into()),
                    new_start_page_token: None,
                },
                4,
            )
            .unwrap();
    }

    let mut store = fixture.open();
    assert_eq!(
        store
            .vault_state()
            .unwrap()
            .unwrap()
            .changes_page_token
            .as_deref(),
        Some("changes-2")
    );
    assert!(matches!(
        store.apply_changes_page(
            "changes-2",
            &ChangesPage {
                changes: Vec::new(),
                next_page_token: Some("next".into()),
                new_start_page_token: Some("ambiguous".into()),
            },
            5,
        ),
        Err(Error::InvalidRemoteEntry)
    ));
    assert_eq!(
        store
            .vault_state()
            .unwrap()
            .unwrap()
            .changes_page_token
            .as_deref(),
        Some("changes-2")
    );
}

#[test]
fn expired_changes_cursor_marks_rescan_required_without_remote_mutation() {
    let fixture = Fixture::new();
    let mut store = ready_store(&fixture);
    // Return to a draining state through a fresh, read-only initial scan.
    store.mark_rescan_required(10).unwrap();
    store.begin_initial_scan("start-2", 11).unwrap();
    store
        .apply_scan_page(
            None,
            &ScanPage {
                entries: Vec::new(),
                next_page_token: None,
            },
            12,
        )
        .unwrap();
    let mut drive = MockDrive::default();
    assert!(matches!(
        advance_initial_sync(&mut store, &mut drive, 13),
        Err(Error::Remote(_))
    ));
    // A provider's explicit expired-cursor code gets a dedicated disposition.
    let mut expired = ExpiredCursorDrive;
    assert!(matches!(
        advance_initial_sync(&mut store, &mut expired, 14),
        Err(Error::RescanRequired)
    ));
    let state = store.vault_state().unwrap().unwrap();
    assert_eq!(state.phase, SyncPhase::NeedStartToken);
    assert!(state.rescan_required);
    assert!(state.durable_cursor.is_none());
}

#[test]
fn restart_resumes_from_last_committed_scan_page() {
    let fixture = Fixture::new();
    {
        let mut store = fixture.open();
        store
            .bind_remote_root(&binding("account-a", "remote-root"), 1)
            .unwrap();
        store.begin_initial_scan("start-1", 2).unwrap();
        store
            .apply_scan_page(
                None,
                &ScanPage {
                    entries: vec![file("file-a", "A.md", "rev-a", b'a')],
                    next_page_token: Some("scan-2".into()),
                },
                3,
            )
            .unwrap();
    }

    let mut store = fixture.open();
    let state = store.vault_state().unwrap().unwrap();
    assert_eq!(state.phase, SyncPhase::Scanning);
    assert_eq!(state.scan_page_token.as_deref(), Some("scan-2"));
    assert!(matches!(
        store.apply_scan_page(
            None,
            &ScanPage {
                entries: vec![file("wrong-page", "wrong.md", "rev-wrong", b'a')],
                next_page_token: None,
            },
            4,
        ),
        Err(Error::CursorMismatch)
    ));
    assert_eq!(store.remote_entry_count().unwrap(), 1);
    let mut drive = MockDrive {
        scans: VecDeque::from([(
            Some("scan-2".into()),
            ScanPage {
                entries: vec![file("file-b", "B.md", "rev-b", b'b')],
                next_page_token: None,
            },
        )]),
        ..MockDrive::default()
    };
    assert_eq!(
        advance_initial_sync(&mut store, &mut drive, 4).unwrap(),
        InitialSyncProgress::ScanComplete
    );
    assert_eq!(store.remote_entry_count().unwrap(), 2);
}

#[test]
fn rejected_page_does_not_advance_and_same_page_can_be_requested_again() {
    let fixture = Fixture::new();
    let mut store = fixture.open();
    store
        .bind_remote_root(&binding("account-a", "remote-root"), 1)
        .unwrap();
    store.begin_initial_scan("start-1", 2).unwrap();
    let mut invalid = MockDrive {
        scans: VecDeque::from([(
            None,
            ScanPage {
                entries: vec![file("secret", ".obsidian/workspace.json", "rev-1", b'a')],
                next_page_token: Some("scan-2".into()),
            },
        )]),
        ..MockDrive::default()
    };
    assert!(matches!(
        advance_initial_sync(&mut store, &mut invalid, 3),
        Err(Error::InvalidPortablePath)
    ));
    assert_eq!(store.vault_state().unwrap().unwrap().scan_page_token, None);
    assert_eq!(store.remote_entry_count().unwrap(), 0);

    let mut valid = MockDrive {
        scans: VecDeque::from([(
            None,
            ScanPage {
                entries: vec![file("file-a", "A.md", "rev-a", b'a')],
                next_page_token: Some("scan-2".into()),
            },
        )]),
        ..MockDrive::default()
    };
    assert_eq!(
        advance_initial_sync(&mut store, &mut valid, 4).unwrap(),
        InitialSyncProgress::ScanPageCommitted
    );
}

#[test]
fn duplicate_remote_paths_remain_visible_as_separate_candidates() {
    let fixture = Fixture::new();
    {
        let mut store = fixture.open();
        store
            .bind_remote_root(&binding("account-a", "remote-root"), 1)
            .unwrap();
        store.begin_initial_scan("start-1", 2).unwrap();
        store
            .apply_scan_page(
                None,
                &ScanPage {
                    entries: vec![
                        file("duplicate-a", "duplicate.md", "rev-a", b'a'),
                        file("duplicate-b", "duplicate.md", "rev-b", b'b'),
                        file("unique", "unique.md", "rev-c", b'c'),
                    ],
                    next_page_token: None,
                },
                3,
            )
            .unwrap();
    }
    let store = fixture.open();
    let first = store.remote_preview(None, 1).unwrap();
    assert!(first.has_more);
    assert_eq!(first.total_entries, 3);
    assert_eq!(first.colliding_entries, 2);
    assert_eq!(first.entries[0].file_id, "duplicate-a");
    assert!(first.entries[0].path_collision);
    let second = store.remote_preview(first.next_after.as_ref(), 1).unwrap();
    assert_eq!(second.entries[0].file_id, "duplicate-b");
    assert!(second.entries[0].path_collision);
    let final_page = store.remote_preview(second.next_after.as_ref(), 1).unwrap();
    assert_eq!(final_page.entries[0].file_id, "unique");
    assert!(!final_page.entries[0].path_collision);
    assert!(!final_page.has_more);
    assert_eq!(store.remote_entry_count().unwrap(), 3);
    assert!(matches!(
        store.remote_preview(
            Some(&RemotePreviewCursor {
                path: "../escape".into(),
                file_id: "duplicate-a".into(),
            }),
            1,
        ),
        Err(Error::InvalidPreviewCursor)
    ));
}

#[test]
fn remote_checksums_are_typed_and_length_checked() {
    assert!(RemoteContentHash::new(RemoteHashAlgorithm::Md5, "a".repeat(32)).is_ok());
    assert!(RemoteContentHash::new(RemoteHashAlgorithm::Sha1, "b".repeat(40)).is_ok());
    assert!(RemoteContentHash::new(RemoteHashAlgorithm::Sha256, "c".repeat(64)).is_ok());
    assert!(matches!(
        RemoteContentHash::new(RemoteHashAlgorithm::Md5, "d".repeat(64)),
        Err(Error::InvalidRemoteEntry)
    ));
    assert!(matches!(
        RemoteContentHash::new(RemoteHashAlgorithm::Sha256, "A".repeat(64)),
        Err(Error::InvalidRemoteEntry)
    ));
}

#[test]
fn queue_is_exactly_idempotent_and_rejects_protected_paths_and_collisions() {
    let fixture = Fixture::new();
    let mut store = ready_store(&fixture);
    let operation = Uuid::new_v4();
    let job = QueueJob::new(
        operation,
        QueueJobKind::Upload,
        "Notes/ไทย.md",
        None,
        None,
        Some(hash(b'a')),
        10,
    )
    .unwrap();
    assert_eq!(store.enqueue_job(&job).unwrap(), EnqueueOutcome::Enqueued);
    assert_eq!(
        store.enqueue_job(&job).unwrap(),
        EnqueueOutcome::AlreadyPresent
    );
    let different = QueueJob::new(
        operation,
        QueueJobKind::Upload,
        "Notes/different.md",
        None,
        None,
        Some(hash(b'a')),
        10,
    )
    .unwrap();
    assert!(matches!(
        store.enqueue_job(&different),
        Err(Error::QueueCollision)
    ));
    assert!(matches!(
        QueueJob::new(
            Uuid::new_v4(),
            QueueJobKind::Upload,
            ".trash/deleted.md",
            None,
            None,
            Some(hash(b'b')),
            10,
        ),
        Err(Error::InvalidPortablePath)
    ));
    let move_job = QueueJob::new(
        Uuid::new_v4(),
        QueueJobKind::Move,
        "Notes/source.md",
        Some("Archive/destination.md".into()),
        Some("remote-source".into()),
        None,
        11,
    )
    .unwrap();
    assert_eq!(move_job.destination_path(), Some("Archive/destination.md"));
    assert!(matches!(
        QueueJob::new(
            Uuid::new_v4(),
            QueueJobKind::Move,
            "Notes/source.md",
            None,
            Some("remote-source".into()),
            None,
            11,
        ),
        Err(Error::InvalidPortablePath)
    ));
    for kind in [
        QueueJobKind::Download,
        QueueJobKind::Move,
        QueueJobKind::Trash,
    ] {
        let destination = (kind == QueueJobKind::Move).then(|| "Archive/A.md".into());
        assert!(matches!(
            QueueJob::new(
                Uuid::new_v4(),
                kind,
                "Notes/A.md",
                destination,
                None,
                None,
                12,
            ),
            Err(Error::InvalidRemoteId)
        ));
    }
    assert_eq!(store.queue_count().unwrap(), 1);
}

#[test]
fn interrupted_running_job_requires_reconciliation_after_reopen() {
    let fixture = Fixture::new();
    let operation = Uuid::new_v4();
    {
        let mut store = ready_store(&fixture);
        let job = QueueJob::new(
            operation,
            QueueJobKind::Upload,
            "A.md",
            None,
            None,
            Some(hash(b'a')),
            10,
        )
        .unwrap();
        store.enqueue_job(&job).unwrap();
        let claimed = store.claim_next_job(10).unwrap().unwrap();
        assert_eq!(claimed.state(), JobState::Running);
    }

    let mut reopened = fixture.open();
    let recovered = reopened.job(operation).unwrap().unwrap();
    assert_eq!(recovered.state(), JobState::NeedsReconcile);
    assert_eq!(
        recovered.last_error_code(),
        Some("interrupted_unknown_outcome")
    );
    assert!(reopened.claim_next_job(100).unwrap().is_none());
    reopened
        .schedule_retry(operation, 200, "remote_absence_verified")
        .unwrap();
    assert!(reopened.claim_next_job(199).unwrap().is_none());
    assert_eq!(
        reopened.claim_next_job(200).unwrap().unwrap().state(),
        JobState::Running
    );
}

#[test]
fn live_second_store_is_rejected_without_recovering_running_job() {
    let fixture = Fixture::new();
    let operation = Uuid::new_v4();
    let mut first = ready_store(&fixture);
    let job = QueueJob::new(
        operation,
        QueueJobKind::Upload,
        "A.md",
        None,
        None,
        Some(hash(b'a')),
        10,
    )
    .unwrap();
    first.enqueue_job(&job).unwrap();
    assert_eq!(
        first.claim_next_job(10).unwrap().unwrap().state(),
        JobState::Running
    );

    assert!(matches!(
        SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
        Err(Error::SyncLeaseHeld)
    ));
    assert_eq!(
        first.job(operation).unwrap().unwrap().state(),
        JobState::Running
    );

    drop(first);
    let reopened = fixture.open();
    assert_eq!(
        reopened.job(operation).unwrap().unwrap().state(),
        JobState::NeedsReconcile
    );
}

#[test]
fn verified_completion_and_history_commit_together() {
    let fixture = Fixture::new();
    let mut store = ready_store(&fixture);
    let operation = Uuid::new_v4();
    let job = QueueJob::new(
        operation,
        QueueJobKind::Download,
        "A.md",
        None,
        Some("remote-a".into()),
        None,
        10,
    )
    .unwrap();
    store.enqueue_job(&job).unwrap();
    assert!(matches!(
        store.complete_verified_job(operation, "download_verified", 11),
        Err(Error::InvalidStateTransition)
    ));
    store.claim_next_job(10).unwrap().unwrap();
    store
        .complete_verified_job(operation, "download_verified", 11)
        .unwrap();
    assert_eq!(store.queue_count().unwrap(), 0);
    assert_eq!(store.history_count().unwrap(), 1);
    assert_eq!(
        store.job(operation).unwrap().unwrap().state(),
        JobState::Completed
    );
    assert_eq!(
        store.enqueue_job(&job).unwrap(),
        EnqueueOutcome::AlreadyCompleted
    );
    let conflicting = QueueJob::new(
        operation,
        QueueJobKind::Download,
        "B.md",
        None,
        Some("remote-a".into()),
        None,
        10,
    )
    .unwrap();
    assert!(matches!(
        store.enqueue_job(&conflicting),
        Err(Error::QueueCollision)
    ));
    assert_eq!(store.queue_count().unwrap(), 0);
    assert_eq!(store.history_count().unwrap(), 1);
}

#[test]
fn cursor_batch_survives_restart_and_never_commits_partial_local_work() {
    let fixture = Fixture::new();
    let batch_id = Uuid::new_v4();
    {
        let mut store = ready_store(&fixture);
        store
            .begin_change_batch(
                batch_id,
                "durable-1",
                "durable-2",
                ["write-note", "write-attachment"],
            )
            .unwrap();
        store.begin_local_mutation(batch_id, "write-note").unwrap();
        store
            .mark_local_mutation_committed(batch_id, "write-note")
            .unwrap();
        assert!(matches!(
            store.commit_change_batch(batch_id, 20),
            Err(Error::LocalMutationIncomplete)
        ));
        assert_eq!(
            store
                .vault_state()
                .unwrap()
                .unwrap()
                .durable_cursor
                .as_deref(),
            Some("durable-1")
        );
    }

    let mut reopened = fixture.open();
    let active = reopened.active_change_batch().unwrap().unwrap();
    assert_eq!(active.declared_mutations, 2);
    assert_eq!(active.applying_mutations, 0);
    assert_eq!(active.committed_mutations, 1);
    reopened
        .begin_local_mutation(batch_id, "write-attachment")
        .unwrap();
    reopened
        .mark_local_mutation_committed(batch_id, "write-attachment")
        .unwrap();
    assert!(matches!(
        reopened.commit_change_batch(batch_id, 21),
        Err(Error::LocalMutationIncomplete)
    ));
    assert_eq!(
        reopened
            .vault_state()
            .unwrap()
            .unwrap()
            .durable_cursor
            .as_deref(),
        Some("durable-1")
    );
    assert!(reopened.active_change_batch().unwrap().is_some());
}

#[test]
fn applying_local_mutation_survives_restart_and_requires_reconciliation() {
    let fixture = Fixture::new();
    let batch_id = Uuid::new_v4();
    {
        let mut store = ready_store(&fixture);
        store
            .begin_change_batch(batch_id, "durable-1", "durable-2", ["write-note"])
            .unwrap();
        store.begin_local_mutation(batch_id, "write-note").unwrap();
    }

    let mut reopened = fixture.open();
    let active = reopened.active_change_batch().unwrap().unwrap();
    assert_eq!(active.applying_mutations, 1);
    assert_eq!(active.committed_mutations, 0);
    assert_eq!(
        reopened.local_mutations(batch_id).unwrap()[0].state,
        LocalMutationState::Applying
    );
    assert!(matches!(
        reopened.begin_local_mutation(batch_id, "write-note"),
        Err(Error::MutationNeedsReconcile)
    ));
    assert!(matches!(
        reopened.abort_change_batch(batch_id),
        Err(Error::MutationNeedsReconcile)
    ));
    assert!(matches!(
        reopened.commit_change_batch(batch_id, 20),
        Err(Error::LocalMutationIncomplete)
    ));
    reopened
        .reset_local_mutation_after_verified_absence(batch_id, "write-note")
        .unwrap();
    reopened
        .begin_local_mutation(batch_id, "write-note")
        .unwrap();
    reopened
        .mark_local_mutation_committed(batch_id, "write-note")
        .unwrap();
    assert!(matches!(
        reopened.commit_change_batch(batch_id, 21),
        Err(Error::LocalMutationIncomplete)
    ));
    assert!(reopened.active_change_batch().unwrap().is_some());
}

#[test]
fn different_remote_binding_is_rejected_without_mutation() {
    assert!(matches!(
        VerifiedRemoteBinding::new("account-a", "remote-a", "account-b", "remote-a"),
        Err(Error::BindingIdentityMismatch)
    ));
    assert!(matches!(
        VerifiedRemoteBinding::new("account-a", "Vault", "account-a", "remote-a"),
        Err(Error::BindingIdentityMismatch)
    ));
    let fixture = Fixture::new();
    let mut store = fixture.open();
    assert_eq!(
        store
            .bind_remote_root(&binding("account-a", "remote-a"), 1)
            .unwrap(),
        BindOutcome::Created
    );
    assert_eq!(
        store
            .bind_remote_root(&binding("account-a", "remote-a"), 2)
            .unwrap(),
        BindOutcome::AlreadyBound
    );
    assert!(matches!(
        store.bind_remote_root(&binding("account-a", "remote-b"), 3),
        Err(Error::BindingCollision)
    ));
    assert!(matches!(
        store.verify_remote_binding(&binding("account-b", "remote-a")),
        Err(Error::BindingCollision)
    ));
    assert_eq!(
        store.vault_state().unwrap().unwrap().remote_root_id,
        "remote-a"
    );
}

#[test]
fn v1_migration_preserves_evidence_and_requires_verified_account_then_rescan() {
    let fixture = Fixture::new();
    let database_path = {
        let mut store = fixture.open();
        store
            .bind_remote_root(&binding("account-before-migration", "remote-root"), 1)
            .unwrap();
        store.begin_initial_scan("legacy-start", 2).unwrap();
        store
            .apply_scan_page(
                None,
                &ScanPage {
                    entries: vec![file("legacy-file", "legacy.md", "legacy-rev", b'a')],
                    next_page_token: None,
                },
                3,
            )
            .unwrap();
        store.database_path().to_path_buf()
    };
    downgrade_database_to_v1(&database_path);

    let mut store = fixture.open();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let migrated = store.vault_state().unwrap().unwrap();
    assert_eq!(migrated.account_id, None);
    assert_eq!(migrated.remote_root_id, "remote-root");
    assert_eq!(migrated.phase, SyncPhase::NeedStartToken);
    assert!(migrated.rescan_required);
    assert_eq!(store.remote_entry_count().unwrap(), 1);
    let mut drive = MockDrive {
        start_token: Some("must-not-be-fetched".into()),
        ..MockDrive::default()
    };
    assert!(matches!(
        advance_initial_sync(&mut store, &mut drive, 4),
        Err(Error::BindingRequiresAccount)
    ));
    assert!(drive.calls.is_empty());
    assert!(matches!(
        store.bind_remote_root(&binding("account-a", "different-root"), 5),
        Err(Error::BindingCollision)
    ));
    assert_eq!(
        store
            .bind_remote_root(&binding("account-a", "remote-root"), 6)
            .unwrap(),
        BindOutcome::LegacyBindingConfirmed
    );
    assert_eq!(
        store.vault_state().unwrap().unwrap().account_id.as_deref(),
        Some("account-a")
    );
    store.begin_initial_scan("fresh-start", 7).unwrap();
    assert_eq!(store.remote_entry_count().unwrap(), 0);
    assert!(!store.vault_state().unwrap().unwrap().rescan_required);
}

#[test]
fn malformed_v1_schema_is_preserved_and_not_partially_migrated() {
    let fixture = Fixture::new();
    let database_path = {
        let store = fixture.open();
        store.database_path().to_path_buf()
    };
    downgrade_database_to_v1(&database_path);
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "DROP INDEX remote_entries_path_idx;
             CREATE INDEX remote_entries_path_idx ON remote_entries(parent_id);",
        )
        .unwrap();
    drop(connection);
    let before = fs::read(&database_path).unwrap();

    assert!(matches!(
        SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
        Err(Error::InvalidSchema)
    ));
    assert_eq!(fs::read(&database_path).unwrap(), before);
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);
    let new_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'scan_frontier'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(new_table_count, 0);
}

#[test]
fn newer_and_partial_schemas_are_preserved_and_rejected() {
    let newer = Fixture::new();
    let newer_path = {
        let store = newer.open();
        store.database_path().to_path_buf()
    };
    rusqlite::Connection::open(&newer_path)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    assert!(matches!(
        SyncStore::open(&newer.app_data, &newer.vault, newer.vault_id),
        Err(Error::UnsupportedSchema(99))
    ));
    assert!(newer_path.exists());

    let partial = Fixture::new();
    let partial_path = {
        let store = partial.open();
        store.database_path().to_path_buf()
    };
    rusqlite::Connection::open(&partial_path)
        .unwrap()
        .execute_batch(
            "DROP TABLE remote_entries;
             CREATE TABLE remote_entries(file_id TEXT PRIMARY KEY NOT NULL);",
        )
        .unwrap();
    assert!(matches!(
        SyncStore::open(&partial.app_data, &partial.vault, partial.vault_id),
        Err(Error::InvalidSchema)
    ));
    assert!(partial_path.exists());

    let weakened = Fixture::new();
    let weakened_path = {
        let store = weakened.open();
        store.database_path().to_path_buf()
    };
    rusqlite::Connection::open(&weakened_path)
        .unwrap()
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TABLE change_batch_mutations;
             CREATE TABLE change_batch_mutations (
                batch_id TEXT NOT NULL,
                mutation_id TEXT NOT NULL,
                state TEXT NOT NULL,
                PRIMARY KEY (batch_id, mutation_id)
             );",
        )
        .unwrap();
    assert!(matches!(
        SyncStore::open(&weakened.app_data, &weakened.vault, weakened.vault_id),
        Err(Error::InvalidSchema)
    ));
    assert!(weakened_path.exists());
}

#[test]
fn negative_schema_version_is_preserved_and_rejected() {
    let fixture = Fixture::new();
    let database_path = {
        let store = fixture.open();
        store.database_path().to_path_buf()
    };
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    connection.pragma_update(None, "user_version", -1).unwrap();
    drop(connection);
    let before = fs::read(&database_path).unwrap();

    assert!(matches!(
        SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
        Err(Error::InvalidSchema)
    ));
    assert_eq!(fs::read(&database_path).unwrap(), before);

    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, -1);
}

#[test]
fn view_only_version_zero_schema_is_preserved_and_rejected() {
    let fixture = Fixture::new();
    let database_path = {
        let store = fixture.open();
        store.database_path().to_path_buf()
    };
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TABLE change_batch_mutations;
             DROP TABLE change_batch;
             DROP TABLE remote_entries;
             DROP TABLE sync_history;
             DROP TABLE sync_jobs;
             DROP TABLE vault_state;
             PRAGMA user_version = 0;
             CREATE VIEW unexpected_schema_object AS SELECT 1 AS value;",
        )
        .unwrap();
    drop(connection);
    let before = fs::read(&database_path).unwrap();

    assert!(matches!(
        SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
        Err(Error::InvalidSchema)
    ));
    assert_eq!(fs::read(&database_path).unwrap(), before);

    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let view_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'view' AND name = 'unexpected_schema_object'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let created_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'vault_state'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(view_count, 1);
    assert_eq!(created_table_count, 0);
}

#[test]
fn corrupt_database_bytes_are_preserved_and_rejected() {
    let fixture = Fixture::new();
    let database_path = {
        let store = fixture.open();
        store.database_path().to_path_buf()
    };
    let corrupt = b"not-a-sqlite-database-preserve-this-evidence";
    fs::write(&database_path, corrupt).unwrap();
    assert!(matches!(
        SyncStore::open(&fixture.app_data, &fixture.vault, fixture.vault_id),
        Err(Error::Database(_) | Error::InvalidSchema)
    ));
    assert_eq!(fs::read(database_path).unwrap(), corrupt);
}

#[test]
fn private_state_root_must_be_disjoint_from_vault() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let vault = root.join("Vault");
    fs::create_dir(&vault).unwrap();
    let nested = vault.join("private");
    fs::create_dir(&nested).unwrap();
    make_private(&nested);
    assert!(matches!(
        SyncStore::open(&nested, &vault, Uuid::new_v4()),
        Err(Error::PrivateStorage(_))
    ));
}
