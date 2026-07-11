use std::fs;

use myvault_core::{
    CoreError, FileRevision, TrashArea, TrashEntryKind, TrashId, TrashPath, Vault, VaultPath,
    WriteIntent,
};
use tempfile::TempDir;

fn fixture() -> (TempDir, Vault) {
    let directory = tempfile::tempdir().expect("temporary vault");
    let canonical = fs::canonicalize(directory.path()).expect("canonical vault");
    let vault = Vault::open(canonical).expect("open vault");
    (directory, vault)
}

fn trash_path(area: TrashArea, kind: TrashEntryKind) -> TrashPath {
    TrashPath::new(area, TrashId::new(), kind).expect("trash path")
}

fn create_parent(root: &TempDir, path: &TrashPath) {
    fs::create_dir_all(
        root.path()
            .join(path.as_vault_path().as_path())
            .parent()
            .expect("trash parent"),
    )
    .expect("create trash parent");
}

#[test]
fn streams_revision_with_explicit_bound_and_verifies_expected() {
    let (root, vault) = fixture();
    let path = VaultPath::new("บันทึก/สวัสดี.md").expect("path");
    fs::create_dir(root.path().join("บันทึก")).expect("parent");
    let bytes = "เนื้อหาภาษาไทย 😀".repeat(8_000).into_bytes();
    fs::write(root.path().join(path.as_path()), &bytes).expect("note");

    let revision = vault.revision(&path, bytes.len()).expect("revision");
    assert_eq!(revision, FileRevision::from_bytes(&bytes));
    vault
        .verify_expected(&path, &revision, bytes.len())
        .expect("matching revision");
    assert!(matches!(
        vault.revision(&path, bytes.len() - 1),
        Err(CoreError::ResourceLimitExceeded {
            resource: "revision bytes",
            ..
        })
    ));
}

#[test]
fn moves_verified_thai_content_to_exact_staging_payload() {
    let (root, vault) = fixture();
    let source = VaultPath::new("โน้ต/ต้นฉบับ.md").expect("source");
    let payload = trash_path(TrashArea::Staging, TrashEntryKind::Payload);
    let bytes = "สวัสดีจากถังขยะ".as_bytes();
    fs::create_dir(root.path().join("โน้ต")).expect("source parent");
    fs::write(root.path().join(source.as_path()), bytes).expect("source");
    create_parent(&root, &payload);
    let expected = FileRevision::from_bytes(bytes);

    vault
        .move_content_to_trash_payload(&source, &payload, &expected)
        .expect("trash move");

    assert!(!root.path().join(source.as_path()).exists());
    assert_eq!(
        fs::read(root.path().join(payload.as_vault_path().as_path())).expect("payload"),
        bytes
    );
}

#[test]
fn restores_verified_item_without_replacing_destination() {
    let (root, vault) = fixture();
    let payload = trash_path(TrashArea::Items, TrashEntryKind::Payload);
    let destination = VaultPath::new("กู้คืน/โน้ต.md").expect("destination");
    let bytes = "ข้อมูลที่กู้คืน".as_bytes();
    create_parent(&root, &payload);
    fs::create_dir(root.path().join("กู้คืน")).expect("destination parent");
    fs::write(root.path().join(payload.as_vault_path().as_path()), bytes).expect("payload");
    let expected = FileRevision::from_bytes(bytes);

    vault
        .restore_trash_payload(&payload, &destination, &expected)
        .expect("restore");

    assert!(!root.path().join(payload.as_vault_path().as_path()).exists());
    assert_eq!(
        fs::read(root.path().join(destination.as_path())).expect("restored"),
        bytes
    );
}

#[test]
fn stale_revision_preserves_source_and_empty_destination() {
    let (root, vault) = fixture();
    let source = VaultPath::new("source.md").expect("source");
    let payload = trash_path(TrashArea::Staging, TrashEntryKind::Payload);
    fs::write(root.path().join(source.as_path()), b"new").expect("source");
    create_parent(&root, &payload);

    let error = vault
        .move_content_to_trash_payload(&source, &payload, &FileRevision::from_bytes(b"old"))
        .expect_err("stale revision");

    assert!(matches!(error, CoreError::StaleRevision { .. }));
    assert_eq!(
        fs::read(root.path().join(source.as_path())).unwrap(),
        b"new"
    );
    assert!(!root.path().join(payload.as_vault_path().as_path()).exists());
}

#[test]
fn collisions_preserve_both_trash_and_content_files() {
    let (root, vault) = fixture();
    let source = VaultPath::new("source.md").expect("source");
    let staging = trash_path(TrashArea::Staging, TrashEntryKind::Payload);
    create_parent(&root, &staging);
    fs::write(root.path().join(source.as_path()), b"source").expect("source");
    fs::write(root.path().join(staging.as_vault_path().as_path()), b"keep").expect("collision");
    let error = vault
        .move_content_to_trash_payload(&source, &staging, &FileRevision::from_bytes(b"source"))
        .expect_err("collision");
    assert!(matches!(error, CoreError::AlreadyExists(_)));
    assert_eq!(
        fs::read(root.path().join(source.as_path())).unwrap(),
        b"source"
    );
    assert_eq!(
        fs::read(root.path().join(staging.as_vault_path().as_path())).unwrap(),
        b"keep"
    );

    let item = trash_path(TrashArea::Items, TrashEntryKind::Payload);
    let destination = VaultPath::new("destination.md").expect("destination");
    create_parent(&root, &item);
    fs::write(root.path().join(item.as_vault_path().as_path()), b"item").expect("item");
    fs::write(root.path().join(destination.as_path()), b"keep destination").expect("destination");
    let error = vault
        .restore_trash_payload(&item, &destination, &FileRevision::from_bytes(b"item"))
        .expect_err("restore collision");
    assert!(matches!(error, CoreError::AlreadyExists(_)));
    assert_eq!(
        fs::read(root.path().join(item.as_vault_path().as_path())).unwrap(),
        b"item"
    );
    assert_eq!(
        fs::read(root.path().join(destination.as_path())).unwrap(),
        b"keep destination"
    );
}

#[test]
fn strict_layout_and_operation_roles_are_enforced() {
    let id = TrashId::new();
    let canonical = format!(".trash/v1/staging/{id}/payload");
    assert_eq!(
        TrashPath::from_portable(&canonical)
            .expect("canonical")
            .as_vault_path()
            .as_str(),
        canonical
    );
    for invalid in [
        format!(".trash//v1/staging/{id}/payload"),
        format!(".trash/v2/staging/{id}/payload"),
        format!(".trash/v1/other/{id}/payload"),
        format!(".trash/v1/staging/{id}/other"),
        format!(".trash/v1/staging/{id}/payload/extra"),
        format!(
            ".trash/v1/staging/{}/payload",
            id.to_string().to_uppercase()
        ),
    ] {
        assert!(matches!(
            TrashPath::from_portable(&invalid),
            Err(CoreError::InvalidTrashPath(_))
        ));
    }

    let (root, vault) = fixture();
    let source = VaultPath::new("source.md").expect("source");
    fs::write(root.path().join(source.as_path()), b"source").expect("source");
    let manifest =
        TrashPath::new(TrashArea::Staging, id, TrashEntryKind::Manifest).expect("manifest path");
    assert!(matches!(
        vault.move_content_to_trash_payload(
            &source,
            &manifest,
            &FileRevision::from_bytes(b"source")
        ),
        Err(CoreError::InvalidTrashPath(_))
    ));
}

#[test]
fn generic_apis_cannot_bypass_trash_boundary() {
    let (root, vault) = fixture();
    let internal = VaultPath::from_portable(".trash/v1/staging/arbitrary/payload")
        .expect("portable internal path");
    fs::create_dir_all(root.path().join(".trash/v1/staging/arbitrary")).expect("parent");
    fs::write(root.path().join(internal.as_path()), b"internal").expect("internal");
    assert!(matches!(
        vault.read(&internal),
        Err(CoreError::TrashAccessDenied(_))
    ));
    assert!(matches!(
        vault.atomic_write(&internal, b"replace", WriteIntent::UserInitiated),
        Err(CoreError::TrashWriteDenied(_))
    ));
    assert!(matches!(
        vault.create_new(&internal, b"new", WriteIntent::UserInitiated),
        Err(CoreError::TrashWriteDenied(_))
    ));

    let source = VaultPath::new("source.md").expect("source");
    fs::write(root.path().join(source.as_path()), b"source").expect("source");
    assert!(matches!(
        vault.atomic_move(&source, &internal, WriteIntent::UserInitiated),
        Err(CoreError::TrashWriteDenied(_))
    ));
    assert_eq!(
        fs::read(root.path().join(source.as_path())).unwrap(),
        b"source"
    );
}

#[test]
fn directory_payload_is_rejected_until_a_bounded_tree_revision_exists() {
    let (root, vault) = fixture();
    let source = VaultPath::new("โฟลเดอร์").expect("source");
    let payload = trash_path(TrashArea::Staging, TrashEntryKind::Payload);
    fs::create_dir(root.path().join(source.as_path())).expect("source directory");
    create_parent(&root, &payload);
    let error = vault
        .move_content_to_trash_payload(&source, &payload, &FileRevision::from_bytes(b""))
        .expect_err("directory revision unsupported");
    assert!(matches!(error, CoreError::RevisionTargetNotFile(_)));
    assert!(root.path().join(source.as_path()).is_dir());
}

#[cfg(unix)]
#[test]
fn symlink_source_and_symlinked_trash_parent_are_rejected() {
    use std::os::unix::fs::symlink;

    let (root, vault) = fixture();
    fs::write(root.path().join("target.md"), b"target").expect("target");
    symlink(root.path().join("target.md"), root.path().join("source.md")).expect("source link");
    let source = VaultPath::new("source.md").expect("source");
    let payload = trash_path(TrashArea::Staging, TrashEntryKind::Payload);
    create_parent(&root, &payload);
    assert!(matches!(
        vault.move_content_to_trash_payload(
            &source,
            &payload,
            &FileRevision::from_bytes(b"target")
        ),
        Err(CoreError::SymlinkRejected(_) | CoreError::Io(_))
    ));

    fs::remove_file(root.path().join("source.md")).expect("remove source link");
    fs::write(root.path().join("source.md"), b"source").expect("source");
    fs::remove_dir_all(root.path().join(".trash")).expect("remove trash");
    fs::create_dir(root.path().join("outside")).expect("outside");
    symlink(root.path().join("outside"), root.path().join(".trash")).expect("trash link");
    assert!(matches!(
        vault.move_content_to_trash_payload(
            &source,
            &payload,
            &FileRevision::from_bytes(b"source")
        ),
        Err(CoreError::SymlinkRejected(_) | CoreError::Io(_))
    ));
    assert!(!root.path().join("outside/v1").exists());
}
