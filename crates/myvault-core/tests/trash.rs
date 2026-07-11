use std::fs;
use std::io::Write;

use myvault_core::{
    CoreError, FileRevision, ManifestDigest, PrepareManifestOutcome, PublishItemOutcome,
    RestoreItemOutcome, TrashArea, TrashId, TrashManifestV1, Vault, VaultPath,
    MAX_TRASH_MANIFEST_BYTES, MAX_TRASH_PAYLOAD_BYTES,
};
use tempfile::TempDir;
use uuid::Uuid;

const TRASH_ID: &str = "11111111-1111-4111-8111-111111111111";
const OPERATION_ID: &str = "22222222-2222-4222-8222-222222222222";
const HELLO_BLAKE3: &str = "ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f";

fn fixture() -> (TempDir, Vault) {
    let root = tempfile::tempdir().expect("temporary vault");
    let canonical = fs::canonicalize(root.path()).expect("canonical vault");
    let vault = Vault::open(canonical).expect("open vault");
    (root, vault)
}

fn id() -> TrashId {
    TrashId::parse(TRASH_ID).expect("trash id")
}

fn manifest(path: &str, bytes: &[u8]) -> TrashManifestV1 {
    TrashManifestV1::new(
        id(),
        Uuid::parse_str(OPERATION_ID).expect("operation id"),
        &VaultPath::from_portable(path).expect("content path"),
        FileRevision::from_bytes(bytes),
        1_234_567_890,
    )
    .expect("manifest")
}

fn staging_directory(root: &TempDir) -> std::path::PathBuf {
    root.path().join(format!(".trash/v1/staging/{TRASH_ID}"))
}

fn write_manifest_raw(root: &TempDir, bytes: &[u8]) {
    let directory = staging_directory(root);
    fs::create_dir_all(&directory).expect("staging directory");
    fs::write(directory.join("manifest.json"), bytes).expect("manifest bytes");
}

fn prepare_staged_file(root: &TempDir, vault: &Vault) -> (TrashManifestV1, ManifestDigest) {
    let source = VaultPath::from_portable("note.md").unwrap();
    fs::write(root.path().join(source.as_path()), b"hello").unwrap();
    let manifest = manifest(source.as_str(), b"hello");
    let digest = manifest.digest().unwrap();
    let store = vault.trash_store();
    store.prepare_staging_manifest(id(), &manifest).unwrap();
    store
        .stage_payload_if_revision(id(), &source, &digest)
        .unwrap();
    (manifest, digest)
}

#[test]
fn canonical_manifest_has_golden_bytes_and_digest() {
    let manifest = manifest("โน้ต.md", b"hello");
    let expected = format!(
        "{{\"version\":1,\"trash_id\":\"{TRASH_ID}\",\"operation_id\":\"{OPERATION_ID}\",\"original_path\":\"โน้ต.md\",\"payload_kind\":\"file\",\"revision\":{{\"hex\":\"{HELLO_BLAKE3}\",\"byte_len\":5}},\"trashed_at_unix_ms\":1234567890}}"
    );
    assert_eq!(manifest.canonical_bytes().unwrap(), expected.as_bytes());
    assert_eq!(
        manifest.digest().unwrap().as_str(),
        "86d6e404184b2a1bdd7de143d32fe4214c8008c7a6293a094af0d0633da64c8f"
    );
}

#[test]
fn canonical_reader_rejects_format_aliases_duplicates_unknown_and_oversize() {
    let (root, vault) = fixture();
    let canonical = manifest("note.md", b"hello").canonical_bytes().unwrap();
    let canonical_text = String::from_utf8(canonical.clone()).unwrap();
    let reordered = canonical_text.replacen("{\"version\":1,\"trash_id\"", "{\"trash_id\"", 1);
    let reordered = reordered.replacen(
        &format!("\"{TRASH_ID}\",\"operation_id\""),
        &format!("\"{TRASH_ID}\",\"version\":1,\"operation_id\""),
        1,
    );
    let variants = [
        format!(" {canonical_text}"),
        reordered,
        canonical_text.replacen("\"version\":1", "\"version\":1,\"version\":1", 1),
        canonical_text.replacen("\"version\":1", "\"version\":1,\"unknown\":true", 1),
        canonical_text.replacen(
            &format!("\"hex\":\"{HELLO_BLAKE3}\",\"byte_len\":5"),
            &format!("\"byte_len\":5,\"hex\":\"{HELLO_BLAKE3}\""),
            1,
        ),
        canonical_text.replacen("\"byte_len\":5", "\"byte_len\":5,\"byte_len\":5", 1),
        canonical_text.replacen("\"byte_len\":5", "\"byte_len\":5,\"nested_unknown\":0", 1),
    ];
    for bytes in variants {
        write_manifest_raw(&root, bytes.as_bytes());
        assert!(matches!(
            vault.trash_store().read_manifest(TrashArea::Staging, id()),
            Err(CoreError::NonCanonicalTrashManifest | CoreError::InvalidTrashManifest(_))
        ));
        fs::remove_file(staging_directory(&root).join("manifest.json")).unwrap();
    }

    write_manifest_raw(&root, &vec![b'x'; MAX_TRASH_MANIFEST_BYTES + 1]);
    assert!(matches!(
        vault.trash_store().read_manifest(TrashArea::Staging, id()),
        Err(CoreError::ResourceLimitExceeded {
            resource: "trash manifest bytes",
            ..
        })
    ));
}

#[test]
fn semantic_reader_rejects_version_kind_path_hash_and_uuid() {
    let (root, vault) = fixture();
    let canonical = String::from_utf8(manifest("note.md", b"hello").canonical_bytes().unwrap())
        .expect("UTF-8 manifest");
    let variants = [
        canonical.replacen("\"version\":1", "\"version\":2", 1),
        canonical.replacen(
            "\"payload_kind\":\"file\"",
            "\"payload_kind\":\"directory\"",
            1,
        ),
        canonical.replacen(
            "\"original_path\":\"note.md\"",
            "\"original_path\":\".trash/x\"",
            1,
        ),
        canonical.replacen(
            "\"original_path\":\"note.md\"",
            "\"original_path\":\".ｔｒａｓｈ/x\"",
            1,
        ),
        canonical.replacen(
            HELLO_BLAKE3,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            1,
        ),
        canonical.replacen(OPERATION_ID, "00000000-0000-0000-0000-000000000000", 1),
        canonical.replacen(TRASH_ID, "33333333-3333-4333-8333-333333333333", 1),
    ];
    for bytes in variants {
        write_manifest_raw(&root, bytes.as_bytes());
        assert!(vault
            .trash_store()
            .read_manifest(TrashArea::Staging, id())
            .is_err());
        fs::remove_file(staging_directory(&root).join("manifest.json")).unwrap();
    }

    let mut oversized_revision = manifest("note.md", b"hello");
    oversized_revision.revision.byte_len = u64::try_from(MAX_TRASH_PAYLOAD_BYTES + 1).unwrap();
    assert!(matches!(
        oversized_revision.canonical_bytes(),
        Err(CoreError::ResourceLimitExceeded {
            resource: "trash payload bytes",
            ..
        })
    ));
}

#[test]
fn trash_id_rejects_aliases_and_nil() {
    assert!(TrashId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").is_ok());
    assert!(TrashId::parse("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA").is_err());
    assert!(TrashId::parse("11111111111141118111111111111111").is_err());
    assert!(TrashId::parse("00000000-0000-0000-0000-000000000000").is_err());
    assert!(ManifestDigest::parse("A".repeat(64)).is_err());
    assert!(ManifestDigest::parse("0".repeat(63)).is_err());
}

#[test]
fn prepare_is_idempotent_preserves_stale_temps_and_detects_collision() {
    let (root, vault) = fixture();
    let store = vault.trash_store();
    let first = manifest("note.md", b"hello");
    assert_eq!(
        store.prepare_staging_manifest(id(), &first).unwrap(),
        PrepareManifestOutcome::Prepared
    );
    let stale = staging_directory(&root).join(".manifest-stale.tmp");
    fs::write(&stale, b"stale").expect("stale temp");
    assert_eq!(
        store.prepare_staging_manifest(id(), &first).unwrap(),
        PrepareManifestOutcome::AlreadyPrepared
    );
    assert_eq!(fs::read(&stale).unwrap(), b"stale");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(staging_directory(&root).join("manifest.json"))
                .unwrap()
                .nlink(),
            1
        );
    }

    let different = manifest("different.md", b"different");
    assert!(matches!(
        store.prepare_staging_manifest(id(), &different),
        Err(CoreError::TrashManifestCollision(_))
    ));
    assert_eq!(
        store.read_manifest(TrashArea::Staging, id()).unwrap(),
        first
    );
}

#[cfg(unix)]
#[test]
fn prepare_rejects_symlinked_internal_parent_and_case_alias() {
    use std::os::unix::fs::symlink;

    let (root, vault) = fixture();
    fs::create_dir(root.path().join("outside")).unwrap();
    symlink(root.path().join("outside"), root.path().join(".trash")).unwrap();
    assert!(vault
        .trash_store()
        .prepare_staging_manifest(id(), &manifest("note.md", b"hello"))
        .is_err());
    assert!(fs::read_dir(root.path().join("outside"))
        .unwrap()
        .next()
        .is_none());

    fs::remove_file(root.path().join(".trash")).unwrap();
    fs::create_dir(root.path().join(".TRASH")).unwrap();
    assert!(matches!(
        vault
            .trash_store()
            .prepare_staging_manifest(id(), &manifest("note.md", b"hello")),
        Err(CoreError::PortablePathCollision { .. })
    ));
}

#[cfg(unix)]
#[test]
fn prepare_and_read_reject_symlink_nonregular_and_hardlinked_manifest() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixDatagram;

    for kind in ["symlink", "directory", "socket", "hardlink"] {
        let (root, vault) = fixture();
        let directory = staging_directory(&root);
        fs::create_dir_all(&directory).unwrap();
        let final_path = directory.join("manifest.json");
        match kind {
            "symlink" => {
                fs::write(root.path().join("outside"), b"outside").unwrap();
                symlink(root.path().join("outside"), &final_path).unwrap();
            }
            "directory" => fs::create_dir(&final_path).unwrap(),
            "socket" => {
                let short = std::path::PathBuf::from(format!(
                    "/tmp/myvault-trash-socket-{}",
                    std::process::id()
                ));
                let _ = fs::remove_file(&short);
                let socket = UnixDatagram::bind(&short).unwrap();
                fs::rename(&short, &final_path).unwrap();
                std::mem::forget(socket);
            }
            "hardlink" => {
                let bytes = manifest("note.md", b"hello").canonical_bytes().unwrap();
                fs::write(&final_path, bytes).unwrap();
                fs::hard_link(&final_path, directory.join("alias.json")).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(vault
            .trash_store()
            .read_manifest(TrashArea::Staging, id())
            .is_err());
    }
}

#[test]
fn stage_payload_is_bound_to_manifest_source_revision_and_digest() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("บันทึก/ต้นฉบับ.md").unwrap();
    fs::create_dir(root.path().join("บันทึก")).unwrap();
    fs::write(root.path().join(source.as_path()), b"hello").unwrap();
    let manifest = manifest(source.as_str(), b"hello");
    let store = vault.trash_store();
    store.prepare_staging_manifest(id(), &manifest).unwrap();

    let wrong_digest = ManifestDigest::parse("0".repeat(64)).unwrap();
    assert!(matches!(
        store.stage_payload_if_revision(id(), &source, &wrong_digest),
        Err(CoreError::TrashManifestDigestMismatch)
    ));
    let wrong_source = VaultPath::from_portable("other.md").unwrap();
    assert!(matches!(
        store.stage_payload_if_revision(id(), &wrong_source, &manifest.digest().unwrap()),
        Err(CoreError::InvalidTrashManifest(_))
    ));
    fs::write(root.path().join(source.as_path()), b"changed").unwrap();
    assert!(matches!(
        store.stage_payload_if_revision(id(), &source, &manifest.digest().unwrap()),
        Err(CoreError::StaleRevision { .. })
    ));
    fs::write(root.path().join(source.as_path()), b"hello").unwrap();
    store
        .stage_payload_if_revision(id(), &source, &manifest.digest().unwrap())
        .expect("stage bound payload");
    assert_eq!(
        fs::read(staging_directory(&root).join("payload")).unwrap(),
        b"hello"
    );
}

#[test]
fn stage_rejects_payload_collision_and_64mib_plus_one_source() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("large.bin").unwrap();
    let manifest = manifest(source.as_str(), b"hello");
    let store = vault.trash_store();
    store.prepare_staging_manifest(id(), &manifest).unwrap();
    fs::write(staging_directory(&root).join("payload"), b"keep").unwrap();
    fs::write(root.path().join(source.as_path()), b"hello").unwrap();
    assert!(matches!(
        store.stage_payload_if_revision(id(), &source, &manifest.digest().unwrap()),
        Err(CoreError::AlreadyExists(_))
    ));
    assert_eq!(
        fs::read(staging_directory(&root).join("payload")).unwrap(),
        b"keep"
    );

    fs::remove_file(staging_directory(&root).join("payload")).unwrap();
    let mut file = fs::File::create(root.path().join(source.as_path())).unwrap();
    file.set_len(u64::try_from(MAX_TRASH_PAYLOAD_BYTES + 1).unwrap())
        .unwrap();
    file.flush().unwrap();
    assert!(matches!(
        store.stage_payload_if_revision(id(), &source, &manifest.digest().unwrap()),
        Err(CoreError::ResourceLimitExceeded {
            resource: "revision bytes",
            ..
        })
    ));
}

#[test]
fn stage_rejects_directory_source() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("folder").unwrap();
    fs::create_dir(root.path().join(source.as_path())).unwrap();
    let manifest = manifest(source.as_str(), b"");
    let store = vault.trash_store();
    store.prepare_staging_manifest(id(), &manifest).unwrap();
    assert!(matches!(
        store.stage_payload_if_revision(id(), &source, &manifest.digest().unwrap()),
        Err(CoreError::RevisionTargetNotFile(_))
    ));
}

#[cfg(unix)]
#[test]
fn stage_rejects_symlink_and_nonregular_source() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixDatagram;

    let (root, vault) = fixture();
    let source = VaultPath::from_portable("source.md").unwrap();
    fs::write(root.path().join("target.md"), b"hello").unwrap();
    symlink(
        root.path().join("target.md"),
        root.path().join(source.as_path()),
    )
    .unwrap();
    let first = manifest(source.as_str(), b"hello");
    vault
        .trash_store()
        .prepare_staging_manifest(id(), &first)
        .unwrap();
    assert!(vault
        .trash_store()
        .stage_payload_if_revision(id(), &source, &first.digest().unwrap())
        .is_err());

    fs::remove_file(root.path().join(source.as_path())).unwrap();
    let short = std::path::PathBuf::from(format!(
        "/tmp/myvault-trash-source-socket-{}",
        std::process::id()
    ));
    let _ = fs::remove_file(&short);
    let socket = UnixDatagram::bind(&short).unwrap();
    fs::rename(&short, root.path().join(source.as_path())).unwrap();
    assert!(vault
        .trash_store()
        .stage_payload_if_revision(id(), &source, &first.digest().unwrap())
        .is_err());
    drop(socket);
}

#[cfg(unix)]
#[test]
fn stage_rejects_multiply_linked_source() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("source.md").unwrap();
    fs::write(root.path().join(source.as_path()), b"hello").unwrap();
    fs::hard_link(
        root.path().join(source.as_path()),
        root.path().join("alias.md"),
    )
    .unwrap();
    let manifest = manifest(source.as_str(), b"hello");
    let store = vault.trash_store();
    store.prepare_staging_manifest(id(), &manifest).unwrap();
    assert!(matches!(
        store.stage_payload_if_revision(id(), &source, &manifest.digest().unwrap()),
        Err(CoreError::InvalidMove { .. })
    ));
    assert!(root.path().join(source.as_path()).exists());
}

#[test]
fn publish_is_atomic_and_idempotent() {
    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    let store = vault.trash_store();
    assert!(matches!(
        store.publish_staging_item(id(), &digest).unwrap(),
        PublishItemOutcome::Published(_)
    ));
    assert!(!staging_directory(&root).exists());
    assert_eq!(
        fs::read(
            root.path()
                .join(format!(".trash/v1/items/{TRASH_ID}/payload"))
        )
        .unwrap(),
        b"hello"
    );
    assert!(matches!(
        store.publish_staging_item(id(), &digest).unwrap(),
        PublishItemOutcome::AlreadyPublished(_)
    ));
}

#[test]
fn publish_rejects_both_neither_bad_digest_manifest_and_revision() {
    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    let items = root.path().join(format!(".trash/v1/items/{TRASH_ID}"));
    fs::create_dir_all(&items).unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));
    fs::remove_dir_all(&items).unwrap();
    fs::remove_dir_all(staging_directory(&root)).unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    assert!(matches!(
        vault
            .trash_store()
            .publish_staging_item(id(), &ManifestDigest::parse("0".repeat(64)).unwrap()),
        Err(CoreError::TrashManifestDigestMismatch)
    ));
    fs::write(staging_directory(&root).join("manifest.json"), b"{").unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashManifest(_))
    ));

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    fs::write(staging_directory(&root).join("payload"), b"changed").unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::StaleRevision { .. })
    ));
}

#[test]
fn publish_enforces_reserved_temp_extras_policy() {
    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    fs::write(
        staging_directory(&root).join(".manifest-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.tmp"),
        b"stale",
    )
    .unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Ok(PublishItemOutcome::Published(_))
    ));

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    for index in 0..32_u128 {
        let uuid = Uuid::from_u128(0x4000_8000_0000_0000_0000_0000_0000_0000_u128 + index);
        fs::write(
            staging_directory(&root).join(format!(".manifest-{uuid}.tmp")),
            b"x",
        )
        .unwrap();
    }
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Ok(PublishItemOutcome::Published(_))
    ));

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    fs::write(staging_directory(&root).join("unexpected"), b"x").unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    fs::write(root.path().join("hardlink-source"), b"x").unwrap();
    fs::hard_link(
        root.path().join("hardlink-source"),
        staging_directory(&root).join(".manifest-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.tmp"),
    )
    .unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    fs::write(
        staging_directory(&root).join(".manifest-AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA.tmp"),
        b"x",
    )
    .unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    let oversized =
        staging_directory(&root).join(".manifest-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.tmp");
    fs::File::create(&oversized)
        .unwrap()
        .set_len((MAX_TRASH_MANIFEST_BYTES + 1) as u64)
        .unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    fs::create_dir(
        staging_directory(&root).join(".manifest-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.tmp"),
    )
    .unwrap();
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    for index in 0..33_u128 {
        let uuid = Uuid::from_u128(0x4000_8000_0000_0000_0000_0000_0000_0000_u128 + index);
        fs::write(
            staging_directory(&root).join(format!(".manifest-{uuid}.tmp")),
            b"x",
        )
        .unwrap();
    }
    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));
}

#[cfg(unix)]
#[test]
fn publish_rejects_reserved_temp_symlink() {
    use std::os::unix::fs::symlink;

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    symlink(
        root.path().join("note.md"),
        staging_directory(&root).join(".manifest-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.tmp"),
    )
    .unwrap();

    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn publish_rejects_non_utf8_extra_name() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    fs::write(
        staging_directory(&root).join(OsString::from_vec(vec![b'.', 0xff])),
        b"x",
    )
    .unwrap();

    assert!(matches!(
        vault.trash_store().publish_staging_item(id(), &digest),
        Err(CoreError::InvalidTrashTopology(_))
    ));
}

#[test]
fn restore_leaves_manifest_and_is_idempotent() {
    let (root, vault) = fixture();
    let (manifest, digest) = prepare_staged_file(&root, &vault);
    let store = vault.trash_store();
    store.publish_staging_item(id(), &digest).unwrap();
    let destination = VaultPath::from_portable(&manifest.original_path).unwrap();
    assert!(matches!(
        store
            .restore_item_if_revision(id(), &destination, &digest)
            .unwrap(),
        RestoreItemOutcome::Restored(_)
    ));
    assert_eq!(fs::read(root.path().join("note.md")).unwrap(), b"hello");
    assert!(root
        .path()
        .join(format!(".trash/v1/items/{TRASH_ID}/manifest.json"))
        .is_file());
    assert!(!root
        .path()
        .join(format!(".trash/v1/items/{TRASH_ID}/payload"))
        .exists());
    assert!(matches!(
        store
            .restore_item_if_revision(id(), &destination, &digest)
            .unwrap(),
        RestoreItemOutcome::AlreadyRestored(_)
    ));
}

#[test]
fn restore_rejects_wrong_destination_and_collision_even_if_identical() {
    let (root, vault) = fixture();
    let (_manifest, digest) = prepare_staged_file(&root, &vault);
    let store = vault.trash_store();
    store.publish_staging_item(id(), &digest).unwrap();
    let wrong = VaultPath::from_portable("wrong.md").unwrap();
    assert!(matches!(
        store.restore_item_if_revision(id(), &wrong, &digest),
        Err(CoreError::InvalidTrashManifest(_))
    ));
    fs::write(root.path().join("note.md"), b"hello").unwrap();
    assert!(matches!(
        store.restore_item_if_revision(
            id(),
            &VaultPath::from_portable("note.md").unwrap(),
            &digest
        ),
        Err(CoreError::AlreadyExists(_))
    ));
    assert!(root
        .path()
        .join(format!(".trash/v1/items/{TRASH_ID}/payload"))
        .is_file());
}
