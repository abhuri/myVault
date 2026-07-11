#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};

use myvault_core::{
    FileRevision, TrashArea, TrashId, Vault, VaultPath, WriteIntent, MAX_TRASH_PAYLOAD_BYTES,
};
use myvault_mutations::{MutationError, MutationService, OperationId, TrashOperation};
use myvault_recovery::{FileRevision as RecoveryRevision, RecoveryJournal, RenameMoveIntent};
use tempfile::TempDir;

fn roots() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temporary = TempDir::new().unwrap();
    let base = temporary.path().canonicalize().unwrap();
    let app = base.join("app");
    let vault = base.join("vault");
    fs::create_dir(&app).unwrap();
    fs::create_dir(&vault).unwrap();
    fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
    (temporary, app, vault)
}

fn setup_note(vault: &Vault, path: &VaultPath, bytes: &[u8]) {
    vault
        .create_new(path, bytes, WriteIntent::UserInitiated)
        .unwrap();
}

fn recovery_revision(revision: &FileRevision) -> RecoveryRevision {
    RecoveryRevision {
        blake3_hex: revision.hex.clone(),
        byte_len: revision.byte_len,
    }
}

fn manifest_and_intent(
    operation: &TrashOperation,
    vault: &Vault,
) -> (myvault_core::TrashManifestV1, RenameMoveIntent) {
    let source = VaultPath::from_portable(operation.source()).unwrap();
    let manifest = myvault_core::TrashManifestV1::new(
        operation.trash_id(),
        operation.operation_id().as_uuid(),
        &source,
        operation.revision().clone(),
        operation.trashed_at_unix_ms(),
    )
    .unwrap();
    let digest = manifest.digest().unwrap();
    let intent = RenameMoveIntent::new_trash(
        operation.operation_id().as_uuid(),
        operation.trash_id().as_uuid(),
        digest.as_str().to_owned(),
        operation.trashed_at_unix_ms(),
        operation.source(),
        recovery_revision(operation.revision()),
    )
    .unwrap();
    assert_eq!(
        vault.revision(&source, MAX_TRASH_PAYLOAD_BYTES).unwrap(),
        *operation.revision()
    );
    (manifest, intent)
}

#[test]
fn fresh_execute_publishes_item_then_completes_without_unlinking_evidence() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let source = VaultPath::from_portable("notes/trash.md").unwrap();
    setup_note(&vault, &source, b"trash me");
    let service = MutationService::new(&vault, &journal);
    let operation = MutationService::plan_trash(&vault, &source, 10).unwrap();

    let outcome = service.execute_trash(&operation).unwrap();

    assert_eq!(outcome.operation_id, operation.operation_id());
    assert!(!vault_path.join(source.as_path()).exists());
    let item = vault
        .trash_store()
        .read_manifest(TrashArea::Items, operation.trash_id())
        .unwrap();
    assert_eq!(item.operation_id, operation.operation_id().as_uuid());
    assert!(vault_path
        .join(format!(".trash/v1/items/{}/payload", operation.trash_id()))
        .exists());
    assert!(app
        .join("operation-journal")
        .join(format!("{}.json", operation.operation_id()))
        .exists());
    assert!(app
        .join("operation-journal/completed")
        .join(format!("{}.json", operation.operation_id()))
        .exists());
}

#[test]
fn retained_retry_crosses_each_public_crash_boundary() {
    for phase in 0..4 {
        let (_temporary, app, vault_path) = roots();
        let vault = Vault::open(&vault_path).unwrap();
        let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
        let source = VaultPath::from_portable(format!("notes/phase-{phase}.md")).unwrap();
        setup_note(&vault, &source, b"phase");
        let service = MutationService::new(&vault, &journal);
        let operation = MutationService::plan_trash(&vault, &source, 20 + phase).unwrap();
        let (manifest, intent) = manifest_and_intent(&operation, &vault);
        let digest = manifest.digest().unwrap();
        let store = vault.trash_store();

        journal.publish(&intent).unwrap();
        if phase >= 1 {
            store
                .prepare_staging_manifest(operation.trash_id(), &manifest)
                .unwrap();
        }
        if phase >= 2 {
            store
                .stage_payload_if_revision(operation.trash_id(), &source, &digest)
                .unwrap();
        }
        if phase >= 3 {
            store
                .publish_staging_item(operation.trash_id(), &digest)
                .unwrap();
        }

        let result = service.retry_trash(&operation);
        assert!(result.is_ok(), "phase {phase}: {result:?}");
    }
}

#[test]
fn new_service_resumes_from_journal_only_or_later_phase() {
    for phase in 0..4 {
        let (_temporary, app, vault_path) = roots();
        let operation_id = {
            let vault = Vault::open(&vault_path).unwrap();
            let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
            let source = VaultPath::from_portable(format!("notes/resume-{phase}.md")).unwrap();
            setup_note(&vault, &source, b"resume");
            let operation = MutationService::plan_trash(&vault, &source, 30 + phase).unwrap();
            let (manifest, intent) = manifest_and_intent(&operation, &vault);
            let digest = manifest.digest().unwrap();
            let store = vault.trash_store();
            journal.publish(&intent).unwrap();
            if phase >= 1 {
                store
                    .prepare_staging_manifest(operation.trash_id(), &manifest)
                    .unwrap();
            }
            if phase >= 2 {
                store
                    .stage_payload_if_revision(operation.trash_id(), &source, &digest)
                    .unwrap();
            }
            if phase >= 3 {
                store
                    .publish_staging_item(operation.trash_id(), &digest)
                    .unwrap();
            }
            operation.operation_id()
        };

        let reopened_vault = Vault::open(&vault_path).unwrap();
        let reopened_journal = RecoveryJournal::open(&app, &vault_path).unwrap();
        MutationService::new(&reopened_vault, &reopened_journal)
            .resume_trash(operation_id)
            .unwrap();
    }
}

#[test]
fn stale_revision_is_preserved_as_core_error() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let source = VaultPath::from_portable("notes/stale.md").unwrap();
    setup_note(&vault, &source, b"before");
    let service = MutationService::new(&vault, &journal);
    let operation = MutationService::plan_trash(&vault, &source, 40).unwrap();
    vault
        .atomic_write(&source, b"after", WriteIntent::UserInitiated)
        .unwrap();

    assert!(matches!(
        service.execute_trash(&operation),
        Err(MutationError::Core(
            myvault_core::CoreError::StaleRevision { .. }
        ))
    ));
    assert!(vault_path.join(source.as_path()).exists());
}

#[test]
fn journal_mismatch_blocks_before_trash_mutation() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let source = VaultPath::from_portable("notes/mismatch.md").unwrap();
    setup_note(&vault, &source, b"mismatch");
    let service = MutationService::new(&vault, &journal);
    let operation = MutationService::plan_trash(&vault, &source, 50).unwrap();
    let different_id = TrashId::new();
    let wrong = RenameMoveIntent::new_trash(
        operation.operation_id().as_uuid(),
        different_id.as_uuid(),
        "0".repeat(64),
        operation.trashed_at_unix_ms(),
        operation.source(),
        recovery_revision(operation.revision()),
    )
    .unwrap();
    journal.publish(&wrong).unwrap();

    assert!(matches!(
        service.retry_trash(&operation),
        Err(MutationError::IntentMismatch)
    ));
    assert!(!vault_path.join(".trash").exists());
    assert!(vault_path.join(source.as_path()).exists());
}

#[test]
fn journal_only_resume_rejects_manifest_digest_before_preparing_manifest() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let source = VaultPath::from_portable("notes/bad-digest.md").unwrap();
    setup_note(&vault, &source, b"digest");
    let operation = MutationService::plan_trash(&vault, &source, 55).unwrap();
    let wrong = RenameMoveIntent::new_trash(
        operation.operation_id().as_uuid(),
        operation.trash_id().as_uuid(),
        "0".repeat(64),
        operation.trashed_at_unix_ms(),
        operation.source(),
        recovery_revision(operation.revision()),
    )
    .unwrap();
    journal.publish(&wrong).unwrap();

    assert!(matches!(
        MutationService::new(&vault, &journal).resume_trash(operation.operation_id()),
        Err(MutationError::IntentMismatch)
    ));
    assert!(!vault_path.join(".trash").exists());
    assert!(vault_path.join(source.as_path()).exists());
}

#[test]
fn non_not_found_manifest_error_wins_over_missing_other_area() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let source = VaultPath::from_portable("notes/poisoned-manifest.md").unwrap();
    setup_note(&vault, &source, b"poison");
    let operation = MutationService::plan_trash(&vault, &source, 60).unwrap();
    let (_, intent) = manifest_and_intent(&operation, &vault);
    journal.publish(&intent).unwrap();
    fs::create_dir_all(vault_path.join(format!(
        ".trash/v1/staging/{}/manifest.json",
        operation.trash_id()
    )))
    .unwrap();

    assert!(matches!(
        MutationService::new(&vault, &journal).resume_trash(operation.operation_id()),
        Err(MutationError::Core(_))
    ));
    assert!(vault_path.join(source.as_path()).exists());
}

#[test]
fn concurrent_journal_only_resumes_converge_without_split_manifest_decisions() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let source = VaultPath::from_portable("notes/concurrent-resume.md").unwrap();
    setup_note(&vault, &source, b"concurrent");
    let operation = MutationService::plan_trash(&vault, &source, 70).unwrap();
    let (_, intent) = manifest_and_intent(&operation, &vault);
    journal.publish(&intent).unwrap();

    let vault = Arc::new(vault);
    let journal = Arc::new(journal);
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let vault = Arc::clone(&vault);
        let journal = Arc::clone(&journal);
        let barrier = Arc::clone(&barrier);
        let operation_id = operation.operation_id();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            MutationService::new(&vault, &journal).resume_trash(operation_id)
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    assert!(!vault_path.join(source.as_path()).exists());
    assert!(vault_path
        .join(format!(".trash/v1/items/{}/payload", operation.trash_id()))
        .exists());
}

#[test]
fn unsupported_evidence_is_unchanged_and_does_not_touch_core() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let operation_id = OperationId::new();
    let bytes =
        format!(r#"{{"version":3,"operation_id":"{operation_id}","opaque":true}}"#).into_bytes();
    let path = app
        .join("operation-journal")
        .join(format!("{operation_id}.json"));
    fs::write(&path, &bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let service = MutationService::new(&vault, &journal);

    assert!(matches!(
        service.resume_trash(operation_id),
        Err(MutationError::UnsupportedEvidence { version: 3, .. })
    ));
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert!(!vault_path.join(".trash").exists());
}
