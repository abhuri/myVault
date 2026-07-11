use std::fs;

use myvault_core::{
    CoreError, InventoryKind, InventoryLimits, Vault, VaultPath, WriteIntent, DEFAULT_READ_LIMIT,
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
    fs::create_dir_all(temp.path().join(".ｏｂｓｉｄｉａｎ/plugins")).expect("compat obsidian");
    fs::write(
        temp.path().join(".ｏｂｓｉｄｉａｎ/plugins/data.json"),
        b"hidden",
    )
    .expect("hidden");
    fs::create_dir_all(temp.path().join(".ｔｒａｓｈ/old")).expect("compat trash");

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
    vault
        .create_new(&path, b"first", WriteIntent::UserInitiated)
        .expect("create");
    let error = vault
        .create_new(&path, b"second", WriteIntent::UserInitiated)
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

#[test]
fn inventory_stops_after_limit_plus_one_without_collecting_the_directory() {
    let (temp, vault) = fixture();
    for index in 0..100 {
        fs::write(temp.path().join(format!("{index:03}.md")), b"note").expect("file");
    }
    assert!(matches!(
        vault.inventory(InventoryLimits {
            max_depth: 64,
            max_entries: 3
        }),
        Err(CoreError::ResourceLimitExceeded {
            resource: "inventory entries",
            limit: 3
        })
    ));
}

#[test]
fn inventory_rejects_case_and_compatibility_path_collisions() {
    let (temp, vault) = fixture();
    fs::write(temp.path().join("Note.md"), b"one").expect("first");
    fs::write(temp.path().join("ｎｏｔｅ.md"), b"two").expect("second");
    assert!(matches!(
        vault.inventory(InventoryLimits::default()),
        Err(CoreError::PortablePathCollision { .. })
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn inventory_rejects_nfc_nfd_path_collisions() {
    let (temp, vault) = fixture();
    fs::write(temp.path().join("Café.md"), b"nfc").expect("nfc");
    fs::write(temp.path().join("Cafe\u{301}.md"), b"nfd").expect("nfd");
    assert!(matches!(
        vault.inventory(InventoryLimits::default()),
        Err(CoreError::PortablePathCollision { .. })
    ));
}

#[test]
fn mutations_reject_case_and_nfd_sibling_collisions() {
    let (temp, vault) = fixture();
    fs::create_dir(temp.path().join("Notes")).expect("existing directory");
    let case_collision = VaultPath::new("notes/new.md").expect("path");
    assert!(matches!(
        vault.create_new(&case_collision, b"note", WriteIntent::UserInitiated),
        Err(CoreError::PortablePathCollision { .. })
    ));

    fs::write(temp.path().join("Cafe\u{301}.md"), b"existing").expect("nfd file");
    let nfc_collision = VaultPath::new("Café.md").expect("path");
    assert!(matches!(
        vault.create_new(&nfc_collision, b"new", WriteIntent::UserInitiated),
        Err(CoreError::PortablePathCollision { .. })
    ));

    let atomic_collision = VaultPath::new("NOTES.md").expect("path");
    fs::write(temp.path().join("notes.md"), b"existing").expect("existing file");
    assert!(matches!(
        vault.atomic_write(&atomic_collision, b"new", WriteIntent::UserInitiated),
        Err(CoreError::PortablePathCollision { .. })
    ));
}

#[test]
fn create_policy_runs_before_parent_or_temp_artifacts() {
    let (temp, vault) = fixture();
    let obsidian = VaultPath::new(".ｏｂｓｉｄｉａｎ/new/plugin.json").expect("path");
    assert!(matches!(
        vault.create_new(&obsidian, b"x", WriteIntent::Automatic),
        Err(CoreError::AutomaticObsidianWriteDenied(_))
    ));
    assert!(!temp.path().join(".ｏｂｓｉｄｉａｎ").exists());

    let trash = VaultPath::new(".ｔｒａｓｈ/new/note.md").expect("path");
    assert!(matches!(
        vault.create_new(&trash, b"x", WriteIntent::UserInitiated),
        Err(CoreError::TrashWriteDenied(_))
    ));
    assert!(!temp.path().join(".ｔｒａｓｈ").exists());
}

#[test]
fn concurrent_portable_collisions_have_only_one_winner() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let (_temp, vault) = fixture();
    let vault = Arc::new(vault);
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = ["Race.md", "ｒａｃｅ.md"]
        .into_iter()
        .map(|name| {
            let vault = Arc::clone(&vault);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                vault.create_new(
                    &VaultPath::new(name).expect("path"),
                    name.as_bytes(),
                    WriteIntent::UserInitiated,
                )
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CoreError::PortablePathCollision { .. })))
            .count(),
        1
    );
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
