#[path = "../src/transfer_store.rs"]
mod transfer_store;

use cap_std::{ambient_authority, fs::Dir};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use transfer_store::{AndroidTransferStore, TransferStoreError, MAX_ANDROID_TRANSFER_BYTES};
use uuid::Uuid;

struct Fixture {
    _temporary: TempDir,
    root_path: PathBuf,
    root: Dir,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary root");
        let root_path = temporary.path().join("no-backup");
        fs::create_dir(&root_path).expect("no-backup root");
        make_private(&root_path);
        let root = Dir::open_ambient_dir(&root_path, ambient_authority()).expect("held root");
        Self {
            _temporary: temporary,
            root_path,
            root,
        }
    }

    fn store(&self, vault_id: Uuid) -> AndroidTransferStore {
        AndroidTransferStore::open(self.root.try_clone().unwrap(), vault_id)
            .expect("transfer store")
    }

    fn stage_path(&self, vault_id: Uuid, operation_id: Uuid) -> PathBuf {
        self.root_path
            .join("guarded-transfer/v1")
            .join(vault_id.to_string())
            .join("staging")
            .join(format!("{operation_id}.stage"))
    }

    fn object_path(&self, vault_id: Uuid, digest: &str) -> PathBuf {
        self.root_path
            .join("guarded-transfer/v1")
            .join(vault_id.to_string())
            .join("objects")
            .join(format!("{digest}.blob"))
    }
}

#[cfg(unix)]
fn make_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(not(unix))]
fn make_private(_path: &Path) {}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verified_stage(
    store: &AndroidTransferStore,
    operation_id: Uuid,
    bytes: &[u8],
) -> transfer_store::VerifiedAndroidStage {
    let mut writer = store.begin_stage(operation_id).unwrap();
    writer.write_all(bytes).unwrap();
    store
        .finish_stage(writer, &sha256(bytes), bytes.len() as u64)
        .unwrap()
}

#[test]
fn stage_round_trip_and_immutable_base_are_exact_and_restart_safe() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let bytes = b"hello private stage";
    let store = fixture.store(vault_id);
    let stage = verified_stage(&store, operation_id, bytes);
    assert_eq!(stage.operation_id(), operation_id);
    assert_eq!(stage.sha256(), sha256(bytes));
    assert_eq!(stage.byte_len(), bytes.len() as u64);
    assert_eq!(store.read_verified_stage(&stage).unwrap(), bytes);
    let base = store.publish_base(&stage).unwrap();
    assert_eq!(base.opaque_ref(), format!("sha256-{}", sha256(bytes)));
    assert_eq!(base.byte_len(), bytes.len() as u64);
    drop(store);

    let reopened = fixture.store(vault_id);
    assert!(matches!(
        reopened.load_verified_stage(operation_id, &sha256(bytes), bytes.len() as u64),
        Err(TransferStoreError::StageUnavailable)
    ));
    assert_eq!(
        fs::read(fixture.object_path(vault_id, &sha256(bytes))).unwrap(),
        bytes
    );
}

#[test]
fn crash_after_base_link_resumes_and_removes_stage_without_copying() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let bytes = b"linked crash evidence";
    let digest = sha256(bytes);
    let store = fixture.store(vault_id);
    let stage = verified_stage(&store, operation_id, bytes);
    fs::hard_link(
        fixture.stage_path(vault_id, operation_id),
        fixture.object_path(vault_id, &digest),
    )
    .unwrap();
    drop(stage);
    drop(store);

    let reopened = fixture.store(vault_id);
    let recovered = reopened
        .load_verified_stage(operation_id, &digest, bytes.len() as u64)
        .unwrap();
    let base = reopened.publish_base(&recovered).unwrap();
    assert_eq!(base.opaque_ref(), format!("sha256-{digest}"));
    assert!(!fixture.stage_path(vault_id, operation_id).exists());
    assert_eq!(
        fs::read(fixture.object_path(vault_id, &digest)).unwrap(),
        bytes
    );
}

#[test]
fn vaults_with_the_same_operation_id_are_isolated() {
    let fixture = Fixture::new();
    let vault_a = Uuid::new_v4();
    let vault_b = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let store_a = fixture.store(vault_a);
    let store_b = fixture.store(vault_b);
    let stage_a = verified_stage(&store_a, operation_id, b"vault-a");
    let stage_b = verified_stage(&store_b, operation_id, b"vault-b");
    assert_eq!(store_a.read_verified_stage(&stage_a).unwrap(), b"vault-a");
    assert_eq!(store_b.read_verified_stage(&stage_b).unwrap(), b"vault-b");
    assert_ne!(
        fixture.stage_path(vault_a, operation_id),
        fixture.stage_path(vault_b, operation_id)
    );
}

#[test]
fn discard_removes_only_strictly_short_exact_operation_stage() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let store = fixture.store(vault_id);
    let partial_id = Uuid::new_v4();
    let mut partial = store.begin_stage(partial_id).unwrap();
    partial.write_all(b"a").unwrap();
    drop(partial);
    store
        .discard_incomplete_stage(partial_id, &sha256(b"abc"), 3)
        .unwrap();
    assert!(!fixture.stage_path(vault_id, partial_id).exists());

    let full_wrong_id = Uuid::new_v4();
    let mut full_wrong = store.begin_stage(full_wrong_id).unwrap();
    full_wrong.write_all(b"xyz").unwrap();
    assert!(matches!(
        store.finish_stage(full_wrong, &sha256(b"abc"), 3),
        Err(TransferStoreError::DigestMismatch)
    ));
    assert!(matches!(
        store.discard_incomplete_stage(full_wrong_id, &sha256(b"abc"), 3),
        Err(TransferStoreError::EvidencePreserved)
    ));
    assert_eq!(
        fs::read(fixture.stage_path(vault_id, full_wrong_id)).unwrap(),
        b"xyz"
    );
    let recovered = store
        .load_verified_stage(full_wrong_id, &sha256(b"xyz"), 3)
        .unwrap();
    assert_eq!(store.read_verified_stage(&recovered).unwrap(), b"xyz");

    let verified_id = Uuid::new_v4();
    let verified = verified_stage(&store, verified_id, b"verified");
    assert!(matches!(
        store.discard_incomplete_stage(verified_id, verified.sha256(), verified.byte_len()),
        Err(TransferStoreError::EvidencePreserved)
    ));
    assert_eq!(store.read_verified_stage(&verified).unwrap(), b"verified");
}

#[test]
fn finish_rejects_same_length_bytes_changed_outside_the_writer() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let store = fixture.store(vault_id);
    let mut writer = store.begin_stage(operation_id).unwrap();
    writer.write_all(b"abc").unwrap();
    fs::write(fixture.stage_path(vault_id, operation_id), b"xyz").unwrap();

    assert!(matches!(
        store.finish_stage(writer, &sha256(b"abc"), 3),
        Err(TransferStoreError::DigestMismatch)
    ));
    assert_eq!(
        fs::read(fixture.stage_path(vault_id, operation_id)).unwrap(),
        b"xyz"
    );
}

#[cfg(unix)]
#[test]
fn oversize_and_hardlinked_stage_evidence_is_preserved() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let store = fixture.store(vault_id);

    let oversize_id = Uuid::new_v4();
    drop(store.begin_stage(oversize_id).unwrap());
    let oversize_path = fixture.stage_path(vault_id, oversize_id);
    let oversize_file = fs::OpenOptions::new()
        .write(true)
        .open(&oversize_path)
        .unwrap();
    oversize_file
        .set_len(MAX_ANDROID_TRANSFER_BYTES + 1)
        .unwrap();
    drop(oversize_file);
    assert!(matches!(
        store.discard_incomplete_stage(
            oversize_id,
            &sha256(b"expected"),
            MAX_ANDROID_TRANSFER_BYTES
        ),
        Err(TransferStoreError::EvidencePreserved)
    ));
    assert_eq!(
        fs::metadata(&oversize_path).unwrap().len(),
        MAX_ANDROID_TRANSFER_BYTES + 1
    );

    let hardlink_id = Uuid::new_v4();
    let mut hardlink = store.begin_stage(hardlink_id).unwrap();
    hardlink.write_all(b"a").unwrap();
    drop(hardlink);
    let hardlink_path = fixture.stage_path(vault_id, hardlink_id);
    let second_link = hardlink_path.with_extension("linked");
    fs::hard_link(&hardlink_path, &second_link).unwrap();
    assert!(store
        .discard_incomplete_stage(hardlink_id, &sha256(b"abc"), 3)
        .is_err());
    assert!(hardlink_path.exists());
    assert!(second_link.exists());
}

#[test]
fn wrong_existing_base_is_never_overwritten_or_deleted() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let store = fixture.store(vault_id);
    let bytes = b"abc";
    let digest = sha256(bytes);
    let first = verified_stage(&store, Uuid::new_v4(), bytes);
    store.publish_base(&first).unwrap();
    let object_path = fixture.object_path(vault_id, &digest);
    fs::write(&object_path, b"xyz").unwrap();

    let second = verified_stage(&store, Uuid::new_v4(), bytes);
    assert!(matches!(
        store.publish_base(&second),
        Err(TransferStoreError::DigestMismatch)
    ));
    assert_eq!(fs::read(&object_path).unwrap(), b"xyz");
}
