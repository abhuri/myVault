use std::fs;

use myvault_core::{Vault, VaultPath, WriteIntent};
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
