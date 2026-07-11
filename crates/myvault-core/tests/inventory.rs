use std::fs;

use myvault_core::{
    CoreError, InventoryKind, InventoryLimits, Vault, VaultPath, DEFAULT_READ_LIMIT,
};
use tempfile::TempDir;

fn fixture() -> (TempDir, Vault) {
    let temp = TempDir::new().expect("temp dir");
    let canonical = fs::canonicalize(temp.path()).expect("canonical temp path");
    let vault = Vault::open(canonical).expect("open vault");
    (temp, vault)
}

#[test]
fn inventories_unicode_spaces_deterministically_and_excludes_internal_trees() {
    let (temp, vault) = fixture();
    fs::create_dir_all(temp.path().join("บันทึก ประจำวัน")).expect("content dir");
    fs::write(temp.path().join("บันทึก ประจำวัน/你好.md"), b"note").expect("note");
    fs::write(temp.path().join("z.bin"), b"binary").expect("file");
    fs::create_dir_all(temp.path().join(".obsidian/plugins")).expect("obsidian");
    fs::write(temp.path().join(".obsidian/plugins/data.json"), b"hidden").expect("hidden");
    fs::create_dir_all(temp.path().join(".trash/old")).expect("trash");
    fs::write(temp.path().join(".trash/old/note.md"), b"hidden").expect("hidden");

    let entries = vault
        .inventory(InventoryLimits::default())
        .expect("inventory");
    let paths: Vec<_> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(paths, ["z.bin", "บันทึก ประจำวัน/你好.md"]);
    assert_eq!(entries[0].kind, InventoryKind::File);
    assert_eq!(entries[1].kind, InventoryKind::Markdown);
}

#[test]
fn creates_parent_directories_and_never_overwrites_existing_destination() {
    let (temp, vault) = fixture();
    let path = VaultPath::new("ไทย space/note.md").expect("path");
    vault.create_new(&path, b"first").expect("create");
    let error = vault
        .create_new(&path, b"second")
        .expect_err("must not replace");
    assert!(
        matches!(error, CoreError::Io(ref io) if io.kind() == std::io::ErrorKind::AlreadyExists)
    );
    assert_eq!(
        fs::read(temp.path().join(path.as_path())).expect("read"),
        b"first"
    );
}

#[test]
fn bounded_read_rejects_large_files() {
    let (temp, vault) = fixture();
    fs::write(temp.path().join("large.md"), vec![0_u8; 12]).expect("large file");
    let path = VaultPath::new("large.md").expect("path");
    assert!(matches!(
        vault.read_bounded(&path, 11),
        Err(CoreError::ResourceLimitExceeded {
            resource: "file size",
            limit: 11
        })
    ));
    assert_eq!(vault.read(&path).expect("default bounded read").len(), 12);
    assert_eq!(DEFAULT_READ_LIMIT, 16 * 1024 * 1024);
}

#[test]
fn enforces_inventory_entry_and_depth_limits() {
    let (temp, vault) = fixture();
    fs::create_dir_all(temp.path().join("one/two")).expect("nested");
    fs::write(temp.path().join("one/two/note.md"), b"note").expect("note");
    assert!(matches!(
        vault.inventory(InventoryLimits {
            max_depth: 1,
            max_entries: 100
        }),
        Err(CoreError::ResourceLimitExceeded {
            resource: "inventory depth",
            limit: 1
        })
    ));
    assert!(matches!(
        vault.inventory(InventoryLimits {
            max_depth: 64,
            max_entries: 1
        }),
        Err(CoreError::ResourceLimitExceeded {
            resource: "inventory entries",
            limit: 1
        })
    ));
}

#[cfg(unix)]
#[test]
fn inventory_rejects_symlinks_in_content() {
    use std::os::unix::fs::symlink;

    let (temp, vault) = fixture();
    fs::write(temp.path().join("target.md"), b"target").expect("target");
    symlink("target.md", temp.path().join("link.md")).expect("symlink");
    assert!(matches!(
        vault.inventory(InventoryLimits::default()),
        Err(CoreError::SymlinkRejected(_))
    ));
}
