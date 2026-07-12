#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};

use myvault_core::{CoreError, MoveContentOutcome, Vault, VaultPath, WriteIntent};
use myvault_mutations::{MutationError, MutationService, NormalMoveOperation};
use myvault_recovery::{FileRevision as RecoveryRevision, RecoveryJournal, RenameMoveIntent};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    app: std::path::PathBuf,
    vault_path: std::path::PathBuf,
    vault: Vault,
    journal: RecoveryJournal,
    operation: NormalMoveOperation,
    source: VaultPath,
    destination: VaultPath,
    bytes: Vec<u8>,
}

fn fixture(name: &str) -> Fixture {
    let temporary = TempDir::new().unwrap();
    let base = temporary.path().canonicalize().unwrap();
    let app = base.join("app");
    let vault_path = base.join("vault");
    fs::create_dir(&app).unwrap();
    fs::create_dir(&vault_path).unwrap();
    fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let source = VaultPath::from_portable(format!("บันทึก/{name}-ต้นทาง.md")).unwrap();
    let destination = VaultPath::from_portable(format!("คลัง/{name}-ปลายทาง.md")).unwrap();
    let destination_parent = VaultPath::from_portable("คลัง").unwrap();
    vault
        .create_directories(&destination_parent, WriteIntent::UserInitiated)
        .unwrap();
    let bytes = format!("เนื้อหา-{name}").into_bytes();
    vault
        .create_new(&source, &bytes, WriteIntent::UserInitiated)
        .unwrap();
    let operation = MutationService::plan_normal_move(&vault, &source, &destination).unwrap();
    Fixture {
        _temporary: temporary,
        app,
        vault_path,
        vault,
        journal,
        operation,
        source,
        destination,
        bytes,
    }
}

fn intent(operation: &NormalMoveOperation) -> RenameMoveIntent {
    RenameMoveIntent::new(
        operation.operation_id().as_uuid(),
        operation.source(),
        operation.destination(),
        RecoveryRevision {
            blake3_hex: operation.revision().hex.clone(),
            byte_len: operation.revision().byte_len,
        },
    )
    .unwrap()
}

#[test]
fn fresh_unicode_move_completes_without_unlinking_journal() {
    let fixture = fixture("สด");

    let outcome = MutationService::new(&fixture.vault, &fixture.journal)
        .execute_normal_move(&fixture.operation)
        .unwrap();

    assert!(matches!(outcome.moved, MoveContentOutcome::Moved(_)));
    assert!(!fixture.vault_path.join(fixture.source.as_path()).exists());
    assert_eq!(
        fs::read(fixture.vault_path.join(fixture.destination.as_path())).unwrap(),
        fixture.bytes
    );
    assert!(fixture
        .app
        .join("operation-journal")
        .join(format!("{}.json", fixture.operation.operation_id()))
        .exists());
}

#[test]
fn new_service_refuses_ambiguous_source_only_journal() {
    let fixture = fixture("journal");
    fixture
        .journal
        .publish(&intent(&fixture.operation))
        .unwrap();

    let vault = Vault::open(&fixture.vault_path).unwrap();
    let journal = RecoveryJournal::open(&fixture.app, &fixture.vault_path).unwrap();
    assert!(matches!(
        MutationService::new(&vault, &journal).resume_normal_move(fixture.operation.operation_id()),
        Err(MutationError::Core(CoreError::InvalidMove { .. }))
    ));
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
    assert!(!fixture
        .vault_path
        .join(fixture.destination.as_path())
        .exists());
}

#[test]
fn postmove_precompletion_resume_is_already_moved() {
    let fixture = fixture("postmove");
    fixture
        .journal
        .publish(&intent(&fixture.operation))
        .unwrap();
    fixture
        .vault
        .move_content_file_if_revision(
            &fixture.source,
            &fixture.destination,
            fixture.operation.revision(),
        )
        .unwrap();

    let outcome = MutationService::new(&fixture.vault, &fixture.journal)
        .resume_normal_move(fixture.operation.operation_id())
        .unwrap();
    assert!(matches!(outcome.moved, MoveContentOutcome::AlreadyMoved(_)));
}

#[test]
fn retained_mismatch_blocks_before_move() {
    let fixture = fixture("mismatch");
    let wrong = RenameMoveIntent::new(
        fixture.operation.operation_id().as_uuid(),
        fixture.operation.source(),
        "คลัง/อื่น.md",
        RecoveryRevision {
            blake3_hex: fixture.operation.revision().hex.clone(),
            byte_len: fixture.operation.revision().byte_len,
        },
    )
    .unwrap();
    fixture.journal.publish(&wrong).unwrap();
    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .retry_normal_move(&fixture.operation),
        Err(MutationError::IntentMismatch)
    ));
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
}

#[test]
fn concurrent_source_only_resumes_fail_closed() {
    let fixture = fixture("concurrent");
    fixture
        .journal
        .publish(&intent(&fixture.operation))
        .unwrap();
    let vault = Arc::new(fixture.vault);
    let journal = Arc::new(fixture.journal);
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let vault = Arc::clone(&vault);
        let journal = Arc::clone(&journal);
        let barrier = Arc::clone(&barrier);
        let operation_id = fixture.operation.operation_id();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            MutationService::new(&vault, &journal).resume_normal_move(operation_id)
        }));
    }
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert!(outcomes.iter().all(|outcome| matches!(
        outcome,
        Err(MutationError::Core(CoreError::InvalidMove { .. }))
    )));
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
    assert!(!fixture
        .vault_path
        .join(fixture.destination.as_path())
        .exists());
}

#[test]
fn retained_source_only_aba_does_not_move_recreated_file() {
    let fixture = fixture("aba");
    fixture
        .journal
        .publish(&intent(&fixture.operation))
        .unwrap();
    fixture
        .vault
        .move_content_file_if_revision(
            &fixture.source,
            &fixture.destination,
            fixture.operation.revision(),
        )
        .unwrap();
    let relocated = fixture.vault_path.join("คลัง/relocated.md");
    fs::rename(
        fixture.vault_path.join(fixture.destination.as_path()),
        &relocated,
    )
    .unwrap();
    fs::write(
        fixture.vault_path.join(fixture.source.as_path()),
        &fixture.bytes,
    )
    .unwrap();

    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .resume_normal_move(fixture.operation.operation_id()),
        Err(MutationError::Core(CoreError::InvalidMove { .. }))
    ));
    assert_eq!(
        fs::read(fixture.vault_path.join(fixture.source.as_path())).unwrap(),
        fixture.bytes
    );
    assert_eq!(fs::read(relocated).unwrap(), fixture.bytes);
    assert!(!fixture
        .vault_path
        .join(fixture.destination.as_path())
        .exists());
}

#[test]
fn identical_destination_collision_preserves_both() {
    let fixture = fixture("collision");
    fixture
        .vault
        .create_new(
            &fixture.destination,
            &fixture.bytes,
            WriteIntent::UserInitiated,
        )
        .unwrap();
    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .execute_normal_move(&fixture.operation),
        Err(MutationError::Core(CoreError::AlreadyExists(_)))
    ));
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
    assert!(fixture
        .vault_path
        .join(fixture.destination.as_path())
        .exists());
}

#[test]
fn both_absent_is_data_loss_error() {
    let fixture = fixture("absent");
    fs::remove_file(fixture.vault_path.join(fixture.source.as_path())).unwrap();
    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .execute_normal_move(&fixture.operation),
        Err(MutationError::Core(CoreError::InvalidMove { .. }))
    ));
}

#[test]
fn stale_wrong_revision_paths_and_kind_fail_closed() {
    let stale = fixture("stale");
    stale
        .vault
        .atomic_write(&stale.source, b"changed", WriteIntent::UserInitiated)
        .unwrap();
    assert!(matches!(
        MutationService::new(&stale.vault, &stale.journal).execute_normal_move(&stale.operation),
        Err(MutationError::Core(CoreError::StaleRevision { .. }))
    ));

    for case in 0..2 {
        let fixture = fixture(&format!("wrong-{case}"));
        let wrong = if case == 0 {
            RenameMoveIntent::new(
                fixture.operation.operation_id().as_uuid(),
                fixture.operation.source(),
                fixture.operation.destination(),
                RecoveryRevision::from_bytes(b"wrong"),
            )
            .unwrap()
        } else {
            RenameMoveIntent::new_restore(
                fixture.operation.operation_id().as_uuid(),
                myvault_core::TrashId::new().as_uuid(),
                "0".repeat(64),
                fixture.operation.destination(),
                RecoveryRevision::from_bytes(b"wrong-kind"),
            )
            .unwrap()
        };
        fixture.journal.publish(&wrong).unwrap();
        let result = MutationService::new(&fixture.vault, &fixture.journal)
            .resume_normal_move(fixture.operation.operation_id());
        if case == 0 {
            assert!(matches!(
                result,
                Err(MutationError::Core(CoreError::InvalidMove { .. }))
            ));
        } else {
            assert!(matches!(result, Err(MutationError::InvalidOperation(_))));
        }
        assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
    }
}

#[test]
fn unsupported_evidence_is_unchanged() {
    let fixture = fixture("unsupported");
    let bytes = format!(
        r#"{{"version":3,"operation_id":"{}","opaque":true}}"#,
        fixture.operation.operation_id()
    )
    .into_bytes();
    let path = fixture
        .app
        .join("operation-journal")
        .join(format!("{}.json", fixture.operation.operation_id()));
    fs::write(&path, &bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .resume_normal_move(fixture.operation.operation_id()),
        Err(MutationError::UnsupportedEvidence { version: 3, .. })
    ));
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
}

#[test]
fn missing_destination_parent_is_not_created() {
    let fixture = fixture("missing-parent");
    let parent = fixture
        .vault_path
        .join(fixture.destination.as_path().parent().unwrap());
    fs::remove_dir(&parent).unwrap();
    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .execute_normal_move(&fixture.operation),
        Err(MutationError::Core(CoreError::Io(error)))
            if error.kind() == std::io::ErrorKind::NotFound
    ));
    assert!(!parent.exists());
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
}
