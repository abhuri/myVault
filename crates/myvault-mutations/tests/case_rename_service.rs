#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};

use myvault_core::{CaseRenameOutcome, CoreError, Vault, VaultPath, WriteIntent};
use myvault_mutations::{CaseRenameOperation, MutationError, MutationService, OperationId};
use myvault_recovery::{FileRevision as RecoveryRevision, RecoveryJournal, RenameMoveIntent};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    app: std::path::PathBuf,
    vault_path: std::path::PathBuf,
    vault: Vault,
    journal: RecoveryJournal,
    operation: CaseRenameOperation,
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
    let source = VaultPath::from_portable(format!("บันทึก/{name}-Note.md")).unwrap();
    let destination = VaultPath::from_portable(format!("บันทึก/{name}-note.md")).unwrap();
    let bytes = format!("case-rename-{name}").into_bytes();
    vault
        .create_new(&source, &bytes, WriteIntent::UserInitiated)
        .unwrap();
    let operation = MutationService::plan_case_rename(&vault, &source, &destination).unwrap();
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

fn intent(operation: &CaseRenameOperation) -> RenameMoveIntent {
    RenameMoveIntent::new_case_rename(
        operation.operation_id().as_uuid(),
        operation.source(),
        operation.destination(),
        RecoveryRevision {
            blake3_hex: operation.revision().hex.clone(),
            byte_len: operation.revision().byte_len,
        },
        operation.temporary(),
    )
    .unwrap()
}

#[test]
fn planning_is_side_effect_free_and_temp_is_deterministic() {
    let fixture = fixture("plan");
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
    assert!(!fixture
        .vault_path
        .join(fixture.operation.temporary())
        .exists());
    let expected_name = format!(
        ".mvcr-{}.tmp",
        fixture.operation.operation_id().as_uuid().simple()
    );
    assert!(fixture.operation.temporary().ends_with(&expected_name));
    assert!(!fixture
        .app
        .join("operation-journal")
        .join(format!("{}.json", fixture.operation.operation_id()))
        .exists());
}

#[test]
fn planning_rejects_a_hardlinked_source() {
    let fixture = fixture("plan-hardlink");
    fs::hard_link(
        fixture.vault_path.join(fixture.source.as_path()),
        fixture.vault_path.join("บันทึก/plan-hardlink-alias.md"),
    )
    .unwrap();

    assert!(matches!(
        MutationService::plan_case_rename(&fixture.vault, &fixture.source, &fixture.destination,),
        Err(MutationError::Core(CoreError::InvalidMove { .. }))
    ));
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
    let destination_name = fixture.destination.as_path().file_name().unwrap();
    assert!(!fs::read_dir(fixture.vault_path.join("บันทึก"))
        .unwrap()
        .any(|entry| entry.unwrap().file_name() == destination_name));
}

#[test]
fn fresh_case_rename_completes() {
    let fixture = fixture("fresh");
    let outcome = MutationService::new(&fixture.vault, &fixture.journal)
        .execute_case_rename(&fixture.operation)
        .unwrap();
    assert!(matches!(outcome.renamed, CaseRenameOutcome::Renamed(_)));
    assert_eq!(
        fs::read(fixture.vault_path.join(fixture.destination.as_path())).unwrap(),
        fixture.bytes
    );
}

#[test]
fn temporary_only_resume_finishes_second_phase() {
    let fixture = fixture("temp");
    fixture
        .journal
        .publish(&intent(&fixture.operation))
        .unwrap();
    fs::rename(
        fixture.vault_path.join(fixture.source.as_path()),
        fixture.vault_path.join(fixture.operation.temporary()),
    )
    .unwrap();
    let outcome = MutationService::new(&fixture.vault, &fixture.journal)
        .resume_case_rename(fixture.operation.operation_id())
        .unwrap();
    assert!(matches!(
        outcome.renamed,
        CaseRenameOutcome::ResumedFromTemporary(_)
    ));
}

#[test]
fn destination_only_resume_is_idempotent() {
    let fixture = fixture("done");
    fixture
        .journal
        .publish(&intent(&fixture.operation))
        .unwrap();
    fs::rename(
        fixture.vault_path.join(fixture.source.as_path()),
        fixture.vault_path.join(fixture.destination.as_path()),
    )
    .unwrap();
    let outcome = MutationService::new(&fixture.vault, &fixture.journal)
        .resume_case_rename(fixture.operation.operation_id())
        .unwrap();
    assert!(matches!(
        outcome.renamed,
        CaseRenameOutcome::AlreadyRenamed(_)
    ));
}

#[test]
fn retained_source_only_aba_is_rejected() {
    let fixture = fixture("aba");
    fixture
        .journal
        .publish(&intent(&fixture.operation))
        .unwrap();
    let temporary = VaultPath::from_portable(fixture.operation.temporary()).unwrap();
    fixture
        .vault
        .case_rename_content_file_if_revision(
            &fixture.source,
            &fixture.destination,
            &temporary,
            fixture.operation.revision(),
        )
        .unwrap();
    let relocated = fixture.vault_path.join("บันทึก/aba-relocated.md");
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
            .resume_case_rename(fixture.operation.operation_id()),
        Err(MutationError::Core(CoreError::InvalidMove { .. }))
    ));
    assert_eq!(
        fs::read(fixture.vault_path.join(fixture.source.as_path())).unwrap(),
        fixture.bytes
    );
    assert_eq!(fs::read(relocated).unwrap(), fixture.bytes);
}

#[test]
fn retained_intent_mismatch_is_rejected_before_mutation() {
    let fixture = fixture("mismatch");
    let wrong_temp = format!("บันทึก/.mvcr-{}.tmp", OperationId::new().as_uuid().simple());
    let wrong = RenameMoveIntent::new_case_rename(
        fixture.operation.operation_id().as_uuid(),
        fixture.operation.source(),
        fixture.operation.destination(),
        RecoveryRevision {
            blake3_hex: fixture.operation.revision().hex.clone(),
            byte_len: fixture.operation.revision().byte_len,
        },
        wrong_temp,
    )
    .unwrap();
    fixture.journal.publish(&wrong).unwrap();
    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .retry_case_rename(&fixture.operation),
        Err(MutationError::IntentMismatch)
    ));
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
}

#[test]
fn retained_destination_mismatch_is_rejected_before_mutation() {
    let fixture = fixture("destination-mismatch");
    let wrong_destination = "บันทึก/destination-mismatch-NOTE.md";
    let wrong = RenameMoveIntent::new_case_rename(
        fixture.operation.operation_id().as_uuid(),
        fixture.operation.source(),
        wrong_destination,
        RecoveryRevision {
            blake3_hex: fixture.operation.revision().hex.clone(),
            byte_len: fixture.operation.revision().byte_len,
        },
        fixture.operation.temporary(),
    )
    .unwrap();
    fixture.journal.publish(&wrong).unwrap();
    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .retry_case_rename(&fixture.operation),
        Err(MutationError::IntentMismatch)
    ));
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
}

#[test]
fn wrong_kind_is_rejected_before_mutation() {
    let fixture = fixture("kind");
    let wrong = RenameMoveIntent::new(
        fixture.operation.operation_id().as_uuid(),
        "other/from.md",
        "other/to.md",
        RecoveryRevision {
            blake3_hex: fixture.operation.revision().hex.clone(),
            byte_len: fixture.operation.revision().byte_len,
        },
    )
    .unwrap();
    fixture.journal.publish(&wrong).unwrap();
    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .resume_case_rename(fixture.operation.operation_id()),
        Err(MutationError::InvalidOperation(_))
    ));
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
}

#[test]
fn unsupported_evidence_is_rejected_before_mutation() {
    let fixture = fixture("unsupported");
    fixture
        .journal
        .publish(&intent(&fixture.operation))
        .unwrap();
    let path = fixture
        .app
        .join("operation-journal")
        .join(format!("{}.json", fixture.operation.operation_id()));
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"version\":4", "\"version\":99", 1)).unwrap();
    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .resume_case_rename(fixture.operation.operation_id()),
        Err(MutationError::UnsupportedEvidence { version: 99, .. })
    ));
    assert!(fixture.vault_path.join(fixture.source.as_path()).exists());
}

#[test]
fn concurrent_temporary_only_resumes_converge() {
    let fixture = fixture("concurrent");
    fixture
        .journal
        .publish(&intent(&fixture.operation))
        .unwrap();
    fs::rename(
        fixture.vault_path.join(fixture.source.as_path()),
        fixture.vault_path.join(fixture.operation.temporary()),
    )
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
            MutationService::new(&vault, &journal).resume_case_rename(operation_id)
        }));
    }
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap().renamed)
        .collect::<Vec<_>>();
    assert!(outcomes
        .iter()
        .any(|outcome| matches!(outcome, CaseRenameOutcome::ResumedFromTemporary(_))));
    assert!(outcomes
        .iter()
        .any(|outcome| matches!(outcome, CaseRenameOutcome::AlreadyRenamed(_))));
}
