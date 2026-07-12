#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};

use myvault_core::{CoreError, RestoreItemOutcome, Vault, VaultPath, WriteIntent};
use myvault_mutations::{MutationError, MutationService, RestoreOperation};
use myvault_recovery::{FileRevision as RecoveryRevision, RecoveryJournal, RenameMoveIntent};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    app: std::path::PathBuf,
    vault_path: std::path::PathBuf,
    vault: Vault,
    journal: RecoveryJournal,
    operation: RestoreOperation,
    original: VaultPath,
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
    let original = VaultPath::from_portable(format!("notes/{name}.md")).unwrap();
    let bytes = format!("content-{name}").into_bytes();
    vault
        .create_new(&original, &bytes, WriteIntent::UserInitiated)
        .unwrap();
    let trash = MutationService::plan_trash(&vault, &original, 100).unwrap();
    MutationService::new(&vault, &journal)
        .execute_trash(&trash)
        .unwrap();
    let operation = MutationService::plan_restore(&vault, trash.trash_id()).unwrap();
    Fixture {
        _temporary: temporary,
        app,
        vault_path,
        vault,
        journal,
        operation,
        original,
        bytes,
    }
}

fn restore_intent(operation: &RestoreOperation) -> RenameMoveIntent {
    RenameMoveIntent::new_restore(
        operation.operation_id().as_uuid(),
        operation.trash_id().as_uuid(),
        operation.manifest_digest().to_owned(),
        operation.destination(),
        RecoveryRevision {
            blake3_hex: operation.revision().hex.clone(),
            byte_len: operation.revision().byte_len,
        },
    )
    .unwrap()
}

fn item_payload(fixture: &Fixture) -> std::path::PathBuf {
    fixture.vault_path.join(format!(
        ".trash/v1/items/{}/payload",
        fixture.operation.trash_id()
    ))
}

#[test]
fn fresh_restore_is_original_path_only_and_retains_manifest() {
    let fixture = fixture("fresh");

    let outcome = MutationService::new(&fixture.vault, &fixture.journal)
        .execute_restore(&fixture.operation)
        .unwrap();

    assert!(matches!(outcome.restored, RestoreItemOutcome::Restored(_)));
    assert_eq!(
        fs::read(fixture.vault_path.join(fixture.original.as_path())).unwrap(),
        fixture.bytes
    );
    assert!(!item_payload(&fixture).exists());
    assert!(fixture
        .vault_path
        .join(format!(
            ".trash/v1/items/{}/manifest.json",
            fixture.operation.trash_id()
        ))
        .exists());
}

#[test]
fn new_service_resumes_journal_only_restore() {
    let fixture = fixture("journal-only");
    fixture
        .journal
        .publish(&restore_intent(&fixture.operation))
        .unwrap();
    let operation_id = fixture.operation.operation_id();

    let reopened_vault = Vault::open(&fixture.vault_path).unwrap();
    let reopened_journal = RecoveryJournal::open(&fixture.app, &fixture.vault_path).unwrap();
    MutationService::new(&reopened_vault, &reopened_journal)
        .resume_restore(operation_id)
        .unwrap();

    assert!(fixture.vault_path.join(fixture.original.as_path()).exists());
}

#[test]
fn postmove_precompletion_resume_is_already_restored() {
    let fixture = fixture("postmove");
    let intent = restore_intent(&fixture.operation);
    fixture.journal.publish(&intent).unwrap();
    let digest =
        myvault_core::ManifestDigest::parse(fixture.operation.manifest_digest().to_owned())
            .unwrap();
    fixture
        .vault
        .trash_store()
        .restore_item_if_revision(fixture.operation.trash_id(), &fixture.original, &digest)
        .unwrap();

    let outcome = MutationService::new(&fixture.vault, &fixture.journal)
        .resume_restore(fixture.operation.operation_id())
        .unwrap();

    assert!(matches!(
        outcome.restored,
        RestoreItemOutcome::AlreadyRestored(_)
    ));
}

#[test]
fn retained_intent_mismatch_blocks_with_payload_untouched() {
    let fixture = fixture("mismatch");
    let wrong = RenameMoveIntent::new_restore(
        fixture.operation.operation_id().as_uuid(),
        fixture.operation.trash_id().as_uuid(),
        "0".repeat(64),
        fixture.operation.destination(),
        RecoveryRevision {
            blake3_hex: fixture.operation.revision().hex.clone(),
            byte_len: fixture.operation.revision().byte_len,
        },
    )
    .unwrap();
    fixture.journal.publish(&wrong).unwrap();

    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal).retry_restore(&fixture.operation),
        Err(MutationError::IntentMismatch)
    ));
    assert!(item_payload(&fixture).exists());
    assert!(!fixture.vault_path.join(fixture.original.as_path()).exists());
}

#[test]
fn concurrent_resumes_converge_without_overwrite() {
    let fixture = fixture("concurrent");
    fixture
        .journal
        .publish(&restore_intent(&fixture.operation))
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
            MutationService::new(&vault, &journal).resume_restore(operation_id)
        }));
    }
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap().restored)
        .collect::<Vec<_>>();
    assert!(outcomes
        .iter()
        .any(|outcome| matches!(outcome, RestoreItemOutcome::Restored(_))));
    assert!(outcomes
        .iter()
        .any(|outcome| matches!(outcome, RestoreItemOutcome::AlreadyRestored(_))));
    assert_eq!(
        fs::read(fixture.vault_path.join(fixture.original.as_path())).unwrap(),
        fixture.bytes
    );
}

#[test]
fn identical_destination_collision_preserves_both_files() {
    let fixture = fixture("collision");
    fixture
        .vault
        .create_new(
            &fixture.original,
            &fixture.bytes,
            WriteIntent::UserInitiated,
        )
        .unwrap();

    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal).execute_restore(&fixture.operation),
        Err(MutationError::Core(CoreError::AlreadyExists(_)))
    ));
    assert!(item_payload(&fixture).exists());
    assert_eq!(
        fs::read(fixture.vault_path.join(fixture.original.as_path())).unwrap(),
        fixture.bytes
    );
}

#[test]
fn neither_payload_nor_destination_is_data_loss_error() {
    let fixture = fixture("data-loss");
    let digest =
        myvault_core::ManifestDigest::parse(fixture.operation.manifest_digest().to_owned())
            .unwrap();
    fixture
        .vault
        .trash_store()
        .restore_item_if_revision(fixture.operation.trash_id(), &fixture.original, &digest)
        .unwrap();
    fs::remove_file(fixture.vault_path.join(fixture.original.as_path())).unwrap();

    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal).execute_restore(&fixture.operation),
        Err(MutationError::Core(CoreError::InvalidTrashTopology(_)))
    ));
}

#[test]
fn missing_original_parent_is_not_recreated() {
    let fixture = fixture("missing-parent");
    let parent = fixture
        .vault_path
        .join(fixture.original.as_path().parent().unwrap());
    fs::remove_dir(&parent).unwrap();

    assert!(matches!(
        MutationService::new(&fixture.vault, &fixture.journal)
            .execute_restore(&fixture.operation),
        Err(MutationError::Core(CoreError::Io(error)))
            if error.kind() == std::io::ErrorKind::NotFound
    ));
    assert!(!parent.exists());
    assert!(item_payload(&fixture).exists());
}

#[test]
fn wrong_destination_revision_and_kind_fail_before_restore() {
    for case in 0..3 {
        let fixture = fixture(&format!("wrong-{case}"));
        let intent = match case {
            0 => RenameMoveIntent::new_restore(
                fixture.operation.operation_id().as_uuid(),
                fixture.operation.trash_id().as_uuid(),
                fixture.operation.manifest_digest().to_owned(),
                "notes/another.md",
                RecoveryRevision {
                    blake3_hex: fixture.operation.revision().hex.clone(),
                    byte_len: fixture.operation.revision().byte_len,
                },
            )
            .unwrap(),
            1 => RenameMoveIntent::new_restore(
                fixture.operation.operation_id().as_uuid(),
                fixture.operation.trash_id().as_uuid(),
                fixture.operation.manifest_digest().to_owned(),
                fixture.operation.destination(),
                RecoveryRevision::from_bytes(b"wrong"),
            )
            .unwrap(),
            2 => RenameMoveIntent::new(
                fixture.operation.operation_id().as_uuid(),
                "notes/source.md",
                "notes/destination.md",
                RecoveryRevision::from_bytes(b"wrong-kind"),
            )
            .unwrap(),
            _ => unreachable!(),
        };
        fixture.journal.publish(&intent).unwrap();
        let result = MutationService::new(&fixture.vault, &fixture.journal)
            .resume_restore(fixture.operation.operation_id());
        assert!(matches!(
            result,
            Err(MutationError::IntentMismatch | MutationError::InvalidOperation(_))
        ));
        assert!(item_payload(&fixture).exists());
        assert!(!fixture.vault_path.join(fixture.original.as_path()).exists());
    }
}

#[test]
fn unsupported_evidence_is_untouched_and_payload_remains() {
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
            .resume_restore(fixture.operation.operation_id()),
        Err(MutationError::UnsupportedEvidence { version: 3, .. })
    ));
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert!(item_payload(&fixture).exists());
}
