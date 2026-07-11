#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;

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
fn operation_ids_are_canonical_and_nonnil() {
    let id = OperationId::new();
    assert!(!id.as_uuid().is_nil());
    assert_eq!(OperationId::parse(&id.to_string()).unwrap(), id);
    assert!(OperationId::parse(&id.to_string().to_uppercase()).is_err());
    assert!(OperationId::parse("00000000-0000-0000-0000-000000000000").is_err());
}

#[test]
fn planning_is_bounded_and_has_no_persistent_side_effects() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let source = VaultPath::from_portable("notes/plan.md").unwrap();
    setup_note(&vault, &source, b"plan");
    let before = fs::read_dir(app.join("operation-journal")).unwrap().count();

    let service = MutationService::new(&vault, &journal);
    let operation = service.plan_trash(&source, 1_700_000_000_000).unwrap();

    assert_eq!(operation.source(), source.as_str());
    assert!(!operation.operation_id().as_uuid().is_nil());
    assert!(!operation.trash_id().as_uuid().is_nil());
    assert!(!vault_path.join(".trash").exists());
    assert_eq!(
        fs::read_dir(app.join("operation-journal")).unwrap().count(),
        before
    );
}

#[test]
fn fresh_execute_publishes_item_then_completes_without_unlinking_evidence() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let source = VaultPath::from_portable("notes/trash.md").unwrap();
    setup_note(&vault, &source, b"trash me");
    let service = MutationService::new(&vault, &journal);
    let operation = service.plan_trash(&source, 10).unwrap();

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
        let operation = service.plan_trash(&source, 20 + phase).unwrap();
        let (manifest, intent) = manifest_and_intent(&operation, &vault);
        let digest = manifest.digest().unwrap();
        let store = vault.trash_store();

        store
            .prepare_staging_manifest(operation.trash_id(), &manifest)
            .unwrap();
        if phase >= 1 {
            journal.publish(&intent).unwrap();
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

        let result = if phase == 0 {
            service.execute_trash(&operation)
        } else {
            service.retry_trash(&operation)
        };
        assert!(result.is_ok(), "phase {phase}: {result:?}");
    }
}

#[test]
fn resume_reconstructs_from_staging_or_items_manifest() {
    for publish_item in [false, true] {
        let (_temporary, app, vault_path) = roots();
        let vault = Vault::open(&vault_path).unwrap();
        let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
        let source = VaultPath::from_portable(if publish_item {
            "notes/resume-items.md"
        } else {
            "notes/resume-staging.md"
        })
        .unwrap();
        setup_note(&vault, &source, b"resume");
        let service = MutationService::new(&vault, &journal);
        let operation = service.plan_trash(&source, 30).unwrap();
        let (manifest, intent) = manifest_and_intent(&operation, &vault);
        let digest = manifest.digest().unwrap();
        let store = vault.trash_store();
        store
            .prepare_staging_manifest(operation.trash_id(), &manifest)
            .unwrap();
        journal.publish(&intent).unwrap();
        store
            .stage_payload_if_revision(operation.trash_id(), &source, &digest)
            .unwrap();
        if publish_item {
            store
                .publish_staging_item(operation.trash_id(), &digest)
                .unwrap();
        }

        service.resume_trash(operation.operation_id()).unwrap();
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
    let operation = service.plan_trash(&source, 40).unwrap();
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
    let operation = service.plan_trash(&source, 50).unwrap();
    let different_id = TrashId::new();
    let wrong = RenameMoveIntent::new_trash(
        operation.operation_id().as_uuid(),
        different_id.as_uuid(),
        "0".repeat(64),
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
fn unsupported_evidence_is_unchanged_and_does_not_touch_core() {
    let (_temporary, app, vault_path) = roots();
    let vault = Vault::open(&vault_path).unwrap();
    let journal = RecoveryJournal::open(&app, &vault_path).unwrap();
    let operation_id = OperationId::new();
    let bytes =
        format!(r#"{{"version":2,"operation_id":"{operation_id}","opaque":true}}"#).into_bytes();
    let path = app
        .join("operation-journal")
        .join(format!("{operation_id}.json"));
    fs::write(&path, &bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let service = MutationService::new(&vault, &journal);

    assert!(matches!(
        service.resume_trash(operation_id),
        Err(MutationError::UnsupportedEvidence { version: 2, .. })
    ));
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert!(!vault_path.join(".trash").exists());
}
