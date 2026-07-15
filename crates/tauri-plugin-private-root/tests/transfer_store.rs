#[path = "../src/transfer_store.rs"]
mod transfer_store;

#[cfg(not(unix))]
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use transfer_store::{
    AndroidTransferStore, DurabilityHook, RenameHook, TransferStoreError,
    MAX_ANDROID_TRANSFER_BYTES,
};
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
        let root = open_syncable_directory(&root_path);
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

    fn pending_path(&self, vault_id: Uuid, operation_id: Uuid) -> PathBuf {
        self.root_path
            .join("guarded-transfer/v1")
            .join(vault_id.to_string())
            .join("objects")
            .join(format!("{operation_id}.pending"))
    }

    fn store_with_rename_hook(
        &self,
        vault_id: Uuid,
        rename: Arc<dyn RenameHook>,
    ) -> AndroidTransferStore {
        AndroidTransferStore::open_with_hooks(
            self.root.try_clone().unwrap(),
            vault_id,
            Arc::new(NoopDurability),
            rename,
        )
        .expect("transfer store with rename hook")
    }
}

struct NoopDurability;

impl DurabilityHook for NoopDurability {
    fn before_sync(&self, _point: transfer_store::DurabilityPoint) -> transfer_store::Result<()> {
        Ok(())
    }
}

struct CreateDestinationBeforeRename {
    destination: PathBuf,
    bytes: Vec<u8>,
    mode: Option<u32>,
    invoked: Mutex<bool>,
}

impl CreateDestinationBeforeRename {
    fn new(destination: PathBuf, bytes: &[u8], mode: Option<u32>) -> Self {
        Self {
            destination,
            bytes: bytes.to_vec(),
            mode,
            invoked: Mutex::new(false),
        }
    }
}

impl RenameHook for CreateDestinationBeforeRename {
    fn before_rename(&self) -> transfer_store::Result<()> {
        let mut invoked = self.invoked.lock().expect("rename hook lock");
        if !*invoked {
            fs::write(&self.destination, &self.bytes).expect("race destination");
            make_private_file(&self.destination);
            #[cfg(unix)]
            if let Some(mode) = self.mode {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&self.destination, fs::Permissions::from_mode(mode))
                    .expect("race destination mode");
            }
            *invoked = true;
        }
        Ok(())
    }
}

#[cfg(unix)]
struct ReplacePendingBeforeRename {
    pending: PathBuf,
    detached: PathBuf,
    replacement: Vec<u8>,
    invoked: Mutex<bool>,
}

#[cfg(unix)]
impl RenameHook for ReplacePendingBeforeRename {
    fn before_rename(&self) -> transfer_store::Result<()> {
        let mut invoked = self.invoked.lock().expect("rename hook lock");
        if !*invoked {
            fs::rename(&self.pending, &self.detached).expect("detach exact pending source");
            fs::write(&self.pending, &self.replacement).expect("install wrong pending source");
            make_private_file(&self.pending);
            *invoked = true;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn open_syncable_directory(path: &Path) -> Dir {
    Dir::from_std_file(fs::File::open(path).expect("syncable held root"))
}

#[cfg(not(unix))]
fn open_syncable_directory(path: &Path) -> Dir {
    Dir::open_ambient_dir(path, ambient_authority()).expect("held root")
}

#[cfg(unix)]
fn make_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(not(unix))]
fn make_private(_path: &Path) {}

#[cfg(unix)]
fn make_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(not(unix))]
fn make_private_file(_path: &Path) {}

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
fn crash_after_base_publish_resumes_and_removes_stage() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let bytes = b"linked crash evidence";
    let digest = sha256(bytes);
    let store = fixture.store(vault_id);
    let stage = verified_stage(&store, operation_id, bytes);
    fs::copy(
        fixture.stage_path(vault_id, operation_id),
        fixture.object_path(vault_id, &digest),
    )
    .unwrap();
    make_private_file(&fixture.object_path(vault_id, &digest));
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

#[test]
fn exact_and_partial_pending_evidence_resume_without_replacing_final() {
    for pending_bytes in [b"pending-body".as_slice(), b"pending".as_slice()] {
        let fixture = Fixture::new();
        let vault_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let expected = b"pending-body";
        let digest = sha256(expected);
        let store = fixture.store(vault_id);
        let stage = verified_stage(&store, operation_id, expected);
        let pending = fixture.pending_path(vault_id, operation_id);
        fs::write(&pending, pending_bytes).unwrap();
        make_private_file(&pending);

        let base = store.publish_base(&stage).unwrap();
        assert_eq!(base.opaque_ref(), format!("sha256-{digest}"));
        assert!(!pending.exists());
        assert!(!fixture.stage_path(vault_id, operation_id).exists());
        assert_eq!(
            fs::read(fixture.object_path(vault_id, &digest)).unwrap(),
            expected
        );
    }
}

#[test]
fn wrong_or_oversized_pending_evidence_is_preserved() {
    for pending_bytes in [b"xyz".as_slice(), b"oversized".as_slice()] {
        let fixture = Fixture::new();
        let vault_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let expected = b"abc";
        let digest = sha256(expected);
        let store = fixture.store(vault_id);
        let stage = verified_stage(&store, operation_id, expected);
        let pending = fixture.pending_path(vault_id, operation_id);
        fs::write(&pending, pending_bytes).unwrap();
        make_private_file(&pending);

        assert!(matches!(
            store.publish_base(&stage),
            Err(TransferStoreError::DigestMismatch)
        ));
        assert_eq!(fs::read(&pending).unwrap(), pending_bytes);
        assert!(fixture.stage_path(vault_id, operation_id).exists());
        assert!(!fixture.object_path(vault_id, &digest).exists());
    }
}

#[test]
fn pending_evidence_above_the_store_limit_is_preserved() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let expected = b"abc";
    let digest = sha256(expected);
    let store = fixture.store(vault_id);
    let stage = verified_stage(&store, operation_id, expected);
    let pending = fixture.pending_path(vault_id, operation_id);
    let oversized = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&pending)
        .unwrap();
    oversized.set_len(MAX_ANDROID_TRANSFER_BYTES + 1).unwrap();
    drop(oversized);
    make_private_file(&pending);

    assert!(matches!(
        store.publish_base(&stage),
        Err(TransferStoreError::DigestMismatch)
    ));
    assert_eq!(
        fs::metadata(&pending).unwrap().len(),
        MAX_ANDROID_TRANSFER_BYTES + 1
    );
    assert!(fixture.stage_path(vault_id, operation_id).exists());
    assert!(!fixture.object_path(vault_id, &digest).exists());
}

#[cfg(unix)]
#[test]
fn unsafe_pending_evidence_is_preserved() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    for case in ["symlink", "hardlink", "mode"] {
        let fixture = Fixture::new();
        let vault_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let expected = b"abc";
        let digest = sha256(expected);
        let store = fixture.store(vault_id);
        let stage = verified_stage(&store, operation_id, expected);
        let pending = fixture.pending_path(vault_id, operation_id);
        let secondary = pending.with_extension("secondary");
        match case {
            "symlink" => {
                fs::write(&secondary, expected).unwrap();
                symlink(&secondary, &pending).unwrap();
            }
            "hardlink" => {
                fs::write(&pending, expected).unwrap();
                make_private_file(&pending);
                fs::hard_link(&pending, &secondary).unwrap();
            }
            "mode" => {
                fs::write(&pending, expected).unwrap();
                fs::set_permissions(&pending, fs::Permissions::from_mode(0o644)).unwrap();
            }
            _ => unreachable!(),
        }

        assert!(store.publish_base(&stage).is_err(), "case: {case}");
        assert!(pending.symlink_metadata().is_ok(), "case: {case}");
        assert!(
            fixture.stage_path(vault_id, operation_id).exists(),
            "case: {case}"
        );
        assert!(
            !fixture.object_path(vault_id, &digest).exists(),
            "case: {case}"
        );
    }
}

#[test]
fn exact_final_cleans_exact_or_partial_pending_evidence() {
    for pending_bytes in [b"abc".as_slice(), b"a".as_slice()] {
        let fixture = Fixture::new();
        let vault_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let expected = b"abc";
        let digest = sha256(expected);
        let store = fixture.store(vault_id);
        let stage = verified_stage(&store, operation_id, expected);
        let object = fixture.object_path(vault_id, &digest);
        let pending = fixture.pending_path(vault_id, operation_id);
        fs::write(&object, expected).unwrap();
        make_private_file(&object);
        fs::write(&pending, pending_bytes).unwrap();
        make_private_file(&pending);

        store.publish_base(&stage).unwrap();
        assert_eq!(fs::read(&object).unwrap(), expected);
        assert!(!pending.exists());
        assert!(!fixture.stage_path(vault_id, operation_id).exists());
    }
}

#[test]
fn exact_final_preserves_wrong_full_length_pending_evidence() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let expected = b"abc";
    let digest = sha256(expected);
    let store = fixture.store(vault_id);
    let stage = verified_stage(&store, operation_id, expected);
    let object = fixture.object_path(vault_id, &digest);
    let pending = fixture.pending_path(vault_id, operation_id);
    fs::write(&object, expected).unwrap();
    make_private_file(&object);
    fs::write(&pending, b"xyz").unwrap();
    make_private_file(&pending);

    assert!(matches!(
        store.publish_base(&stage),
        Err(TransferStoreError::DigestMismatch)
    ));
    assert_eq!(fs::read(&object).unwrap(), expected);
    assert_eq!(fs::read(&pending).unwrap(), b"xyz");
    assert!(fixture.stage_path(vault_id, operation_id).exists());
}

#[test]
fn rename_race_accepts_only_an_exact_destination() {
    for exact in [true, false] {
        let fixture = Fixture::new();
        let vault_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let expected = b"race-body";
        let digest = sha256(expected);
        let object = fixture.object_path(vault_id, &digest);
        let raced = if exact {
            expected.as_slice()
        } else {
            b"wrong-body".as_slice()
        };
        let hook = Arc::new(CreateDestinationBeforeRename::new(
            object.clone(),
            raced,
            None,
        ));
        let store = fixture.store_with_rename_hook(vault_id, hook);
        let stage = verified_stage(&store, operation_id, expected);
        let pending = fixture.pending_path(vault_id, operation_id);

        let outcome = store.publish_base(&stage);
        if exact {
            outcome.unwrap();
            assert!(!pending.exists());
            assert!(!fixture.stage_path(vault_id, operation_id).exists());
            assert_eq!(fs::read(&object).unwrap(), expected);
        } else {
            assert!(matches!(outcome, Err(TransferStoreError::DigestMismatch)));
            assert_eq!(fs::read(&pending).unwrap(), expected);
            assert!(fixture.stage_path(vault_id, operation_id).exists());
            assert_eq!(fs::read(&object).unwrap(), raced);
        }
    }
}

#[cfg(unix)]
#[test]
fn pending_source_swap_before_rename_never_publishes_replacement() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let expected = b"publish-me";
    let replacement = b"wrong-data";
    let digest = sha256(expected);
    let object = fixture.object_path(vault_id, &digest);
    let pending = fixture.pending_path(vault_id, operation_id);
    let detached = pending.with_extension("detached");
    let hook = Arc::new(ReplacePendingBeforeRename {
        pending: pending.clone(),
        detached: detached.clone(),
        replacement: replacement.to_vec(),
        invoked: Mutex::new(false),
    });
    let store = fixture.store_with_rename_hook(vault_id, hook);
    let stage = verified_stage(&store, operation_id, expected);

    assert!(matches!(
        store.publish_base(&stage),
        Err(TransferStoreError::EvidenceAmbiguous)
    ));
    assert!(!object.exists());
    assert_eq!(fs::read(&pending).unwrap(), replacement);
    assert_eq!(fs::read(&detached).unwrap(), expected);
    assert_eq!(
        fs::read(fixture.stage_path(vault_id, operation_id)).unwrap(),
        expected
    );
}

#[cfg(unix)]
#[test]
fn rename_race_preserves_an_unsafe_destination_and_exact_pending() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let expected = b"race-body";
    let digest = sha256(expected);
    let object = fixture.object_path(vault_id, &digest);
    let hook = Arc::new(CreateDestinationBeforeRename::new(
        object.clone(),
        expected,
        Some(0o644),
    ));
    let store = fixture.store_with_rename_hook(vault_id, hook);
    let stage = verified_stage(&store, operation_id, expected);
    let pending = fixture.pending_path(vault_id, operation_id);

    assert!(store.publish_base(&stage).is_err());
    assert_eq!(fs::read(&pending).unwrap(), expected);
    assert!(fixture.stage_path(vault_id, operation_id).exists());
    assert_eq!(fs::read(&object).unwrap(), expected);
}
