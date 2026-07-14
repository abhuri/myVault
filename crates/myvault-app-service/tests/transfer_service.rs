use std::{fs, io::Write, path::PathBuf};

use myvault_app_service::{AppService, NativeTransferError, VaultSessionId};
use myvault_core::{FileRevision, Sha256Digest, Vault};
use uuid::Uuid;

const MAX_TRANSFER: usize = 8 * 1024 * 1024;

struct Fixture {
    temporary: tempfile::TempDir,
    vault: PathBuf,
    app_data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let vault = temporary.path().join("vault");
        let app_data = temporary.path().join("private-app-data");
        fs::create_dir(&vault).unwrap();
        fs::create_dir(&app_data).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&app_data, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            temporary,
            vault: fs::canonicalize(vault).unwrap(),
            app_data: fs::canonicalize(app_data).unwrap(),
        }
    }

    fn service(&self) -> (AppService, VaultSessionId) {
        let service = AppService::with_app_data_root(&self.app_data);
        let session = service
            .activate_trusted_vault(Vault::open(&self.vault).unwrap())
            .unwrap()
            .session_id
            .unwrap();
        (service, session)
    }
}

#[test]
fn binary_download_stages_publishes_reads_back_and_creates_private_base() {
    let fixture = Fixture::new();
    let (service, session) = fixture.service();
    let bytes = binary_fixture(5 * 1024 * 1024 + 37);
    let digest = Sha256Digest::from_bytes(&bytes);
    let stage = service
        .stage_transfer_download(
            session,
            Uuid::new_v4(),
            &mut bytes.as_slice(),
            digest.as_str(),
            bytes.len() as u64,
            MAX_TRANSFER,
        )
        .unwrap();
    assert_eq!(stage.snapshot().sha256, digest);
    let published = service
        .publish_staged_transfer(session, "แนบ/ข้อมูล.bin", &stage, None, MAX_TRANSFER)
        .unwrap();
    assert_eq!(published.snapshot, *stage.snapshot());
    assert_eq!(
        published.base_ref.opaque_ref(),
        format!("sha256-{}", digest.as_str())
    );
    let opaque = published.base_ref.opaque_ref();
    assert!(opaque
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)));
    assert!(!opaque.contains([':', '/', '\\']));
    assert!(!opaque.contains("://"));
    assert_eq!(fs::read(fixture.vault.join("แนบ/ข้อมูล.bin")).unwrap(), bytes);

    let mut upload = Vec::new();
    let source = service
        .stream_transfer_source(session, "แนบ/ข้อมูล.bin", &mut upload, MAX_TRANSFER)
        .unwrap();
    assert_eq!(source, published.snapshot);
    assert_eq!(upload, bytes);
}

#[test]
fn zero_byte_stage_and_stale_replace_preserve_the_existing_target() {
    let fixture = Fixture::new();
    fs::write(fixture.vault.join("note.md"), b"current").unwrap();
    let (service, session) = fixture.service();
    let digest = Sha256Digest::from_bytes(b"");
    let stage = service
        .stage_transfer_download(
            session,
            Uuid::new_v4(),
            &mut b"".as_slice(),
            digest.as_str(),
            0,
            MAX_TRANSFER,
        )
        .unwrap();
    assert!(matches!(
        service.publish_staged_transfer(
            session,
            "note.md",
            &stage,
            Some(&FileRevision::from_bytes(b"stale")),
            MAX_TRANSFER,
        ),
        Err(NativeTransferError::StaleRevision)
    ));
    assert_eq!(fs::read(fixture.vault.join("note.md")).unwrap(), b"current");
}

#[test]
fn digest_mismatch_and_protected_paths_fail_before_vault_publication() {
    let fixture = Fixture::new();
    let (service, session) = fixture.service();
    assert!(matches!(
        service.stage_transfer_download(
            session,
            Uuid::new_v4(),
            &mut b"actual".as_slice(),
            Sha256Digest::from_bytes(b"different").as_str(),
            6,
            128,
        ),
        Err(NativeTransferError::DigestMismatch)
    ));
    assert!(matches!(
        service.stream_transfer_source(session, ".obsidian/private", &mut Vec::new(), 128),
        Err(NativeTransferError::ProtectedPath)
    ));
}

#[test]
fn upload_stage_rehydrates_chunks_and_publishes_base_without_vault_mutation() {
    let fixture = Fixture::new();
    let bytes = binary_fixture(5 * 1024 * 1024 + 111);
    fs::write(fixture.vault.join("source.bin"), &bytes).unwrap();
    let (service, session) = fixture.service();
    let operation_id = Uuid::new_v4();
    let stage = service
        .stage_transfer_source(session, operation_id, "source.bin", MAX_TRANSFER)
        .unwrap();

    let loaded = service
        .load_transfer_stage(
            session,
            operation_id,
            stage.snapshot().sha256.as_str(),
            stage.snapshot().byte_len,
            MAX_TRANSFER,
        )
        .unwrap();
    assert_eq!(loaded, stage);
    let mut reconstructed = Vec::new();
    let mut offset = 0_u64;
    while offset < loaded.snapshot().byte_len {
        let chunk = service
            .read_verified_stage_chunk(session, &loaded, offset, 1024 * 1024, MAX_TRANSFER)
            .unwrap();
        assert!(!chunk.is_empty());
        offset += u64::try_from(chunk.len()).unwrap();
        reconstructed.extend_from_slice(&chunk);
    }
    assert_eq!(reconstructed, bytes);
    let base = service
        .publish_verified_stage_as_base(session, &loaded, MAX_TRANSFER)
        .unwrap();
    assert_eq!(
        base.opaque_ref(),
        format!("sha256-{}", loaded.snapshot().sha256.as_str())
    );
    assert_eq!(fs::read(fixture.vault.join("source.bin")).unwrap(), bytes);
    assert!(matches!(
        service.load_transfer_stage(
            session,
            operation_id,
            loaded.snapshot().sha256.as_str(),
            loaded.snapshot().byte_len,
            MAX_TRANSFER,
        ),
        Err(NativeTransferError::StageUnavailable)
    ));
}

#[test]
fn network_stage_writer_is_bounded_and_finish_revalidates_session() {
    let fixture = Fixture::new();
    let (service, session) = fixture.service();
    let bytes = b"network body";
    let mut writer = service
        .begin_transfer_stage(session, Uuid::new_v4(), bytes.len())
        .unwrap();
    writer.write_all(bytes).unwrap();
    assert!(writer.write_all(b"x").is_err());

    let other = fixture.temporary.path().join("other-vault");
    fs::create_dir(&other).unwrap();
    service
        .activate_trusted_vault(Vault::open(fs::canonicalize(other).unwrap()).unwrap())
        .unwrap();
    assert!(matches!(
        service.finish_transfer_stage(
            session,
            writer,
            Sha256Digest::from_bytes(bytes).as_str(),
            bytes.len() as u64,
            bytes.len(),
        ),
        Err(NativeTransferError::VaultUnavailable)
    ));
}

#[test]
fn existing_stage_reuses_only_exact_evidence_and_preserves_partial_bytes() {
    let fixture = Fixture::new();
    let (service, session) = fixture.service();
    let operation_id = Uuid::new_v4();
    let bytes = b"complete";
    let digest = Sha256Digest::from_bytes(bytes);
    let mut writer = service
        .begin_transfer_stage(session, operation_id, 64)
        .unwrap();
    writer.write_all(bytes).unwrap();
    let exact = service
        .finish_transfer_stage(session, writer, digest.as_str(), bytes.len() as u64, 64)
        .unwrap();
    let reused = service
        .stage_transfer_download(
            session,
            operation_id,
            &mut b"ignored".as_slice(),
            digest.as_str(),
            bytes.len() as u64,
            64,
        )
        .unwrap();
    assert_eq!(reused, exact);

    let partial_id = Uuid::new_v4();
    let mut partial = service
        .begin_transfer_stage(session, partial_id, 64)
        .unwrap();
    partial.write_all(b"part").unwrap();
    drop(partial);
    assert!(matches!(
        service.stage_transfer_download(
            session,
            partial_id,
            &mut bytes.as_slice(),
            digest.as_str(),
            bytes.len() as u64,
            64,
        ),
        Err(NativeTransferError::DigestMismatch)
    ));
}

#[test]
fn restart_discards_only_the_exact_operations_incomplete_stage() {
    let fixture = Fixture::new();
    let (service, old_session) = fixture.service();
    let operation_id = Uuid::new_v4();
    let other_operation_id = Uuid::new_v4();
    let expected = b"complete downloaded body";
    let digest = Sha256Digest::from_bytes(expected);
    let mut partial = service
        .begin_transfer_stage(old_session, operation_id, 64)
        .unwrap();
    partial.write_all(b"partial").unwrap();
    drop(partial);
    drop(service);

    let (restarted, session) = fixture.service();
    assert!(matches!(
        restarted.discard_incomplete_transfer_stage(
            old_session,
            operation_id,
            digest.as_str(),
            expected.len() as u64,
            64,
        ),
        Err(NativeTransferError::VaultUnavailable)
    ));
    assert!(matches!(
        restarted.discard_incomplete_transfer_stage(
            session,
            other_operation_id,
            digest.as_str(),
            expected.len() as u64,
            64,
        ),
        Err(NativeTransferError::StageUnavailable)
    ));
    assert!(matches!(
        restarted.load_transfer_stage(
            session,
            operation_id,
            digest.as_str(),
            expected.len() as u64,
            64,
        ),
        Err(NativeTransferError::DigestMismatch)
    ));

    restarted
        .discard_incomplete_transfer_stage(
            session,
            operation_id,
            digest.as_str(),
            expected.len() as u64,
            64,
        )
        .unwrap();
    let stage = restarted
        .stage_transfer_download(
            session,
            operation_id,
            &mut expected.as_slice(),
            digest.as_str(),
            expected.len() as u64,
            64,
        )
        .unwrap();
    assert_eq!(stage.snapshot().sha256, digest);
    assert!(fs::read_dir(&fixture.vault).unwrap().next().is_none());
}

#[test]
fn incomplete_stage_recovery_refuses_verified_stage_and_preserves_vault() {
    let fixture = Fixture::new();
    fs::write(fixture.vault.join("existing.bin"), b"vault bytes").unwrap();
    let (service, session) = fixture.service();
    let operation_id = Uuid::new_v4();
    let bytes = b"verified private stage";
    let digest = Sha256Digest::from_bytes(bytes);
    let stage = service
        .stage_transfer_download(
            session,
            operation_id,
            &mut bytes.as_slice(),
            digest.as_str(),
            bytes.len() as u64,
            64,
        )
        .unwrap();

    assert!(matches!(
        service.discard_incomplete_transfer_stage(
            session,
            operation_id,
            digest.as_str(),
            bytes.len() as u64,
            64,
        ),
        Err(NativeTransferError::StageAlreadyExists)
    ));
    assert_eq!(
        service
            .load_transfer_stage(
                session,
                operation_id,
                digest.as_str(),
                bytes.len() as u64,
                64,
            )
            .unwrap(),
        stage
    );
    assert_eq!(
        fs::read(fixture.vault.join("existing.bin")).unwrap(),
        b"vault bytes"
    );
}

#[cfg(unix)]
#[test]
fn incomplete_stage_recovery_refuses_hardlinked_private_evidence() {
    let fixture = Fixture::new();
    let (service, session) = fixture.service();
    let operation_id = Uuid::new_v4();
    let expected = b"complete";
    let digest = Sha256Digest::from_bytes(expected);
    let mut partial = service
        .begin_transfer_stage(session, operation_id, 64)
        .unwrap();
    partial.write_all(b"part").unwrap();
    drop(partial);

    let vault_store = single_child(&fixture.app_data.join("guarded-transfer/v1"));
    let stage_path = vault_store
        .join("staging")
        .join(format!("{operation_id}.part"));
    let retained = vault_store.join("objects/retained-test-evidence.blob");
    fs::hard_link(&stage_path, &retained).unwrap();

    assert!(matches!(
        service.discard_incomplete_transfer_stage(
            session,
            operation_id,
            digest.as_str(),
            expected.len() as u64,
            64,
        ),
        Err(NativeTransferError::PrivateStoreUnavailable)
    ));
    assert_eq!(fs::read(stage_path).unwrap(), b"part");
    assert_eq!(fs::read(retained).unwrap(), b"part");
}

#[cfg(unix)]
#[test]
fn hardlink_crash_state_recovers_idempotently_and_parent_swap_fails_closed() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let (service, session) = fixture.service();
    let bytes = b"base bytes";
    let digest = Sha256Digest::from_bytes(bytes);
    let operation_id = Uuid::new_v4();
    let stage = service
        .stage_transfer_download(
            session,
            operation_id,
            &mut bytes.as_slice(),
            digest.as_str(),
            bytes.len() as u64,
            64,
        )
        .unwrap();
    let vault_store = single_child(&fixture.app_data.join("guarded-transfer/v1"));
    let stage_path = vault_store
        .join("staging")
        .join(format!("{operation_id}.part"));
    let object_path = vault_store
        .join("objects")
        .join(format!("{}.blob", digest.as_str()));
    fs::hard_link(&stage_path, &object_path).unwrap();
    let base = service
        .publish_verified_stage_as_base(session, &stage, 64)
        .unwrap();
    assert_eq!(base.opaque_ref(), format!("sha256-{}", digest.as_str()));
    assert!(!stage_path.exists());

    let mut writer = service
        .begin_transfer_stage(session, Uuid::new_v4(), 64)
        .unwrap();
    writer.write_all(b"swap").unwrap();
    let staging = vault_store.join("staging");
    let retained = vault_store.join("staging-retained");
    fs::rename(&staging, &retained).unwrap();
    let outside = fixture.temporary.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, &staging).unwrap();
    assert!(matches!(
        service.finish_transfer_stage(
            session,
            writer,
            Sha256Digest::from_bytes(b"swap").as_str(),
            4,
            64,
        ),
        Err(NativeTransferError::PrivateStoreUnavailable)
    ));
    assert!(fs::read_dir(outside).unwrap().next().is_none());
}

fn binary_fixture(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| u8::try_from((index * 193 + 29) % 256).unwrap())
        .collect()
}

fn single_child(parent: &std::path::Path) -> PathBuf {
    let entries = fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    entries.into_iter().next().unwrap()
}
