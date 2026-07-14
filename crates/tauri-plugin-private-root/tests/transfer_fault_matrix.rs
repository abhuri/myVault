#[path = "../src/transfer_store.rs"]
mod transfer_store;

#[cfg(not(unix))]
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use transfer_store::{AndroidTransferStore, DurabilityHook, DurabilityPoint, TransferStoreError};
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
        AndroidTransferStore::open(self.root.try_clone().unwrap(), vault_id).expect("store")
    }

    fn store_failing_at(&self, vault_id: Uuid, point: DurabilityPoint) -> AndroidTransferStore {
        AndroidTransferStore::open_with_durability(
            self.root.try_clone().unwrap(),
            vault_id,
            Arc::new(FailAt(point)),
        )
        .expect("store")
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
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private mode");
}

#[cfg(not(unix))]
fn make_private(_path: &Path) {}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Copy, Debug)]
enum PrivateCrashBoundary {
    BeforeStagedFsync,
    AfterBaseLinkBeforeStageCleanup,
}

struct FailAt(DurabilityPoint);

impl DurabilityHook for FailAt {
    fn before_sync(&self, point: DurabilityPoint) -> transfer_store::Result<()> {
        if point == self.0 {
            Err(TransferStoreError::Io(io::Error::other(format!(
                "injected durability failure at {point:?}"
            ))))
        } else {
            Ok(())
        }
    }
}

#[test]
fn injected_sync_fault_matrix_preserves_evidence_and_allows_exact_retry() {
    for point in [
        DurabilityPoint::FinishStageFile,
        DurabilityPoint::FinishStageDirectory,
        DurabilityPoint::BaseLinkDirectory,
        DurabilityPoint::BaseVerifiedDirectory,
        DurabilityPoint::BaseCleanupDirectory,
    ] {
        let fixture = Fixture::new();
        let vault_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let expected = b"deterministic-sync-fault";
        let digest = sha256(expected);
        let store = fixture.store_failing_at(vault_id, point);
        let mut writer = store.begin_stage(operation_id).expect("stage");
        writer.write_all(expected).expect("body");

        if matches!(
            point,
            DurabilityPoint::FinishStageFile | DurabilityPoint::FinishStageDirectory
        ) {
            assert!(matches!(
                store.finish_stage(writer, &digest, expected.len() as u64),
                Err(TransferStoreError::Io(_))
            ));
        } else {
            let stage = store
                .finish_stage(writer, &digest, expected.len() as u64)
                .expect("verified stage");
            assert!(matches!(
                store.publish_base(&stage),
                Err(TransferStoreError::Io(_))
            ));
        }

        assert_eq!(
            fs::read(fixture.stage_path(vault_id, operation_id)).ok(),
            (!matches!(point, DurabilityPoint::BaseCleanupDirectory)).then(|| expected.to_vec()),
            "point: {point:?}"
        );
        if matches!(
            point,
            DurabilityPoint::BaseLinkDirectory
                | DurabilityPoint::BaseVerifiedDirectory
                | DurabilityPoint::BaseCleanupDirectory
        ) {
            assert_eq!(
                fs::read(fixture.object_path(vault_id, &digest)).unwrap(),
                expected,
                "point: {point:?}"
            );
        }
        drop(store);

        let reopened = fixture.store(vault_id);
        let recovered =
            match reopened.load_verified_stage(operation_id, &digest, expected.len() as u64) {
                Ok(stage) => stage,
                Err(TransferStoreError::StageUnavailable)
                    if point == DurabilityPoint::BaseCleanupDirectory =>
                {
                    let mut writer = reopened.begin_stage(operation_id).expect("restage");
                    writer.write_all(expected).expect("restaged body");
                    reopened
                        .finish_stage(writer, &digest, expected.len() as u64)
                        .expect("restaged evidence")
                }
                Err(error) => {
                    panic!("point {point:?} did not preserve recoverable evidence: {error}")
                }
            };
        let base = reopened.publish_base(&recovered).expect("idempotent retry");
        assert_eq!(base.opaque_ref(), format!("sha256-{digest}"));
        assert!(!fixture.stage_path(vault_id, operation_id).exists());
        assert_eq!(
            fs::read(fixture.object_path(vault_id, &digest)).unwrap(),
            expected,
            "point: {point:?}"
        );
    }
}

#[test]
fn private_store_crash_matrix_preserves_or_recovers_exact_evidence() {
    for boundary in [
        PrivateCrashBoundary::BeforeStagedFsync,
        PrivateCrashBoundary::AfterBaseLinkBeforeStageCleanup,
    ] {
        let fixture = Fixture::new();
        let vault_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let expected = b"fault-matrix-body";
        let digest = sha256(expected);
        let store = fixture.store(vault_id);

        match boundary {
            PrivateCrashBoundary::BeforeStagedFsync => {
                let mut writer = store.begin_stage(operation_id).expect("stage");
                writer.write_all(&expected[..5]).expect("partial write");
                drop(writer);
                drop(store);

                let reopened = fixture.store(vault_id);
                let load =
                    reopened.load_verified_stage(operation_id, &digest, expected.len() as u64);
                if let Err(error) = load {
                    assert!(
                        matches!(error, TransferStoreError::EvidenceAmbiguous),
                        "boundary: {boundary:?}, error: {error:?}"
                    );
                } else {
                    panic!("boundary unexpectedly verified: {boundary:?}");
                }
                assert_eq!(
                    fs::read(fixture.stage_path(vault_id, operation_id)).unwrap(),
                    &expected[..5],
                    "boundary: {boundary:?}"
                );
                reopened
                    .discard_incomplete_stage(operation_id, &digest, expected.len() as u64)
                    .expect("strictly short stage is safe to discard");
                assert!(!fixture.stage_path(vault_id, operation_id).exists());
            }
            PrivateCrashBoundary::AfterBaseLinkBeforeStageCleanup => {
                let mut writer = store.begin_stage(operation_id).expect("stage");
                writer.write_all(expected).expect("body");
                let stage = store
                    .finish_stage(writer, &digest, expected.len() as u64)
                    .expect("verified stage");
                fs::hard_link(
                    fixture.stage_path(vault_id, operation_id),
                    fixture.object_path(vault_id, &digest),
                )
                .expect("inject completed base-link boundary");
                drop(stage);
                drop(store);

                let reopened = fixture.store(vault_id);
                let recovered = reopened
                    .load_verified_stage(operation_id, &digest, expected.len() as u64)
                    .expect("linked stage recovery");
                assert_eq!(recovered.operation_id(), operation_id);
                assert_eq!(recovered.sha256(), digest);
                assert_eq!(recovered.byte_len(), expected.len() as u64);
                let base = reopened
                    .publish_base(&recovered)
                    .expect("finish publication");
                assert_eq!(base.opaque_ref(), format!("sha256-{digest}"));
                assert_eq!(base.byte_len(), expected.len() as u64);
                assert!(!fixture.stage_path(vault_id, operation_id).exists());
                assert_eq!(
                    fs::read(fixture.object_path(vault_id, &digest)).unwrap(),
                    expected,
                    "boundary: {boundary:?}"
                );
            }
        }
    }
}
