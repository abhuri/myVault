use std::fs;

use myvault_core::{
    FileRevision, TrashArea, TrashId, TrashManifestV1, Vault, VaultPath, WriteIntent,
};
use myvault_mutations::{MutationService, OperationId};
use tempfile::TempDir;

fn vault() -> (TempDir, std::path::PathBuf, Vault) {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("vault");
    fs::create_dir(&root).unwrap();
    let vault = Vault::open(&root).unwrap();
    (temporary, root, vault)
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
fn planning_is_bounded_validated_and_has_no_persistent_side_effects() {
    let (_temporary, root, vault) = vault();
    let source = VaultPath::from_portable("notes/plan.md").unwrap();
    vault
        .create_new(&source, b"plan", WriteIntent::UserInitiated)
        .unwrap();

    let operation = MutationService::plan_trash(&vault, &source, 1_700_000_000_000).unwrap();

    assert_eq!(operation.source(), source.as_str());
    assert!(!operation.operation_id().as_uuid().is_nil());
    assert!(!operation.trash_id().as_uuid().is_nil());
    assert!(!root.join(".trash").exists());
    assert!(root.join(source.as_path()).exists());
    assert!(MutationService::plan_trash(&vault, &source, -1).is_err());
}

#[test]
fn restore_planning_reads_immutable_item_without_moving_payload() {
    let (_temporary, root, vault) = vault();
    let source = VaultPath::from_portable("notes/restore-plan.md").unwrap();
    let bytes = b"restore plan";
    fs::create_dir_all(root.join("notes")).unwrap();
    fs::write(root.join(source.as_path()), bytes).unwrap();
    let trash_id = TrashId::new();
    let manifest = TrashManifestV1::new(
        trash_id,
        OperationId::new().as_uuid(),
        &source,
        FileRevision::from_bytes(bytes),
        100,
    )
    .unwrap();
    let digest = manifest.digest().unwrap();
    let store = vault.trash_store();
    let item = root.join(format!(".trash/v1/items/{trash_id}"));
    fs::create_dir_all(&item).unwrap();
    fs::write(
        item.join("manifest.json"),
        manifest.canonical_bytes().unwrap(),
    )
    .unwrap();
    fs::rename(root.join(source.as_path()), item.join("payload")).unwrap();

    let operation = MutationService::plan_restore(&vault, trash_id).unwrap();

    assert_eq!(operation.trash_id(), trash_id);
    assert_eq!(operation.destination(), source.as_str());
    assert_eq!(operation.revision(), &FileRevision::from_bytes(bytes));
    assert_eq!(operation.manifest_digest(), digest.as_str());
    assert!(!operation.operation_id().as_uuid().is_nil());
    assert!(!root.join(source.as_path()).exists());
    assert!(root
        .join(format!(".trash/v1/items/{trash_id}/payload"))
        .exists());
    assert_eq!(
        store.read_manifest(TrashArea::Items, trash_id).unwrap(),
        manifest
    );
}

#[test]
fn normal_move_planning_is_side_effect_free_and_rejects_case_aliases() {
    let (_temporary, root, vault) = vault();
    let source = VaultPath::from_portable("บันทึก/ต้นทาง.md").unwrap();
    let destination = VaultPath::from_portable("คลัง/ปลายทาง.md").unwrap();
    vault
        .create_new(&source, b"unicode", WriteIntent::UserInitiated)
        .unwrap();

    let operation = MutationService::plan_normal_move(&vault, &source, &destination).unwrap();

    assert_eq!(operation.source(), source.as_str());
    assert_eq!(operation.destination(), destination.as_str());
    assert_eq!(operation.revision(), &FileRevision::from_bytes(b"unicode"));
    assert!(root.join(source.as_path()).exists());
    assert!(!root.join(destination.as_path()).exists());

    let alias_source = VaultPath::from_portable("Note.md").unwrap();
    vault
        .create_new(&alias_source, b"alias", WriteIntent::UserInitiated)
        .unwrap();
    let alias_destination = VaultPath::from_portable("note.md").unwrap();
    assert!(MutationService::plan_normal_move(&vault, &alias_source, &alias_destination).is_err());
}
