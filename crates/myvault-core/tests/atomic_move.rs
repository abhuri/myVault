use std::fs;

use myvault_core::{CoreError, MoveDurability, Vault, VaultPath, WriteIntent};
use tempfile::TempDir;

fn fixture() -> (TempDir, Vault) {
    let directory = tempfile::tempdir().expect("temporary vault");
    let canonical = fs::canonicalize(directory.path()).expect("canonical temporary vault");
    let vault = Vault::open(canonical).expect("open vault");
    (directory, vault)
}

#[test]
fn moves_file_without_replacing_destination() {
    let (root, vault) = fixture();
    fs::write(root.path().join("source.md"), b"source").expect("source");

    let durability = vault
        .atomic_move(
            &VaultPath::new("source.md").expect("source path"),
            &VaultPath::new("moved.md").expect("destination path"),
            WriteIntent::UserInitiated,
        )
        .expect("move");

    #[cfg(not(windows))]
    assert_eq!(durability, MoveDurability::FullySynced);
    #[cfg(windows)]
    assert!(matches!(
        durability,
        MoveDurability::FullySynced | MoveDurability::DirectorySyncUnsupported
    ));
    assert!(!root.path().join("source.md").exists());
    assert_eq!(
        fs::read(root.path().join("moved.md")).expect("moved"),
        b"source"
    );
}

#[test]
fn existing_destination_is_preserved() {
    let (root, vault) = fixture();
    fs::write(root.path().join("source.md"), b"source").expect("source");
    fs::write(root.path().join("destination.md"), b"keep").expect("destination");

    let error = vault
        .atomic_move(
            &VaultPath::new("source.md").expect("source path"),
            &VaultPath::new("destination.md").expect("destination path"),
            WriteIntent::UserInitiated,
        )
        .expect_err("must not replace");

    assert!(matches!(error, CoreError::AlreadyExists(_)));
    assert_eq!(
        fs::read(root.path().join("source.md")).expect("source"),
        b"source"
    );
    assert_eq!(
        fs::read(root.path().join("destination.md")).expect("destination"),
        b"keep"
    );
}

#[test]
fn moves_directory_between_held_parents_with_thai_names() {
    let (root, vault) = fixture();
    fs::create_dir_all(root.path().join("ต้นทาง/โฟลเดอร์")).expect("source directory");
    fs::create_dir(root.path().join("ปลายทาง")).expect("destination parent");
    fs::write(root.path().join("ต้นทาง/โฟลเดอร์/โน้ต.md"), "สวัสดี").expect("note");

    vault
        .atomic_move(
            &VaultPath::new("ต้นทาง/โฟลเดอร์").expect("source path"),
            &VaultPath::new("ปลายทาง/ย้ายแล้ว").expect("destination path"),
            WriteIntent::UserInitiated,
        )
        .expect("directory move");

    assert!(!root.path().join("ต้นทาง/โฟลเดอร์").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("ปลายทาง/ย้ายแล้ว/โน้ต.md")).expect("moved note"),
        "สวัสดี"
    );
}

#[test]
fn same_path_is_an_existing_destination_and_source_survives() {
    let (root, vault) = fixture();
    fs::write(root.path().join("same.md"), b"same").expect("source");
    let same = VaultPath::new("same.md").expect("path");

    let error = vault
        .atomic_move(&same, &same, WriteIntent::UserInitiated)
        .expect_err("same path is not a move");

    assert!(matches!(error, CoreError::AlreadyExists(_)));
    assert_eq!(
        fs::read(root.path().join("same.md")).expect("source"),
        b"same"
    );
}

#[test]
fn rejects_directory_move_into_own_descendant_before_syscall() {
    let (root, vault) = fixture();
    fs::create_dir_all(root.path().join("notes/child")).expect("directory fixture");

    let error = vault
        .atomic_move(
            &VaultPath::new("notes").expect("source path"),
            &VaultPath::new("notes/child/moved").expect("destination path"),
            WriteIntent::UserInitiated,
        )
        .expect_err("descendant move must fail");

    assert!(matches!(error, CoreError::InvalidMove { .. }));
    assert!(root.path().join("notes/child").is_dir());
}

#[cfg(unix)]
#[test]
fn rejects_non_file_and_non_directory_source() {
    use std::os::unix::net::UnixDatagram;

    let (root, vault) = fixture();
    let socket_path = root.path().join("local.socket");
    let _socket = UnixDatagram::bind(&socket_path).expect("socket fixture");

    let error = vault
        .atomic_move(
            &VaultPath::new("local.socket").expect("source path"),
            &VaultPath::new("moved.socket").expect("destination path"),
            WriteIntent::UserInitiated,
        )
        .expect_err("special source must fail");

    assert!(matches!(error, CoreError::InvalidMove { .. }));
    assert!(socket_path.exists());
}
