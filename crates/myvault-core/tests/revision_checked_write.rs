use std::fs;
use std::sync::{Arc, Barrier};

use myvault_core::{CoreError, FileRevision, ReplaceContentOutcome, Vault, VaultPath};

fn fixture() -> (tempfile::TempDir, Vault) {
    let root = tempfile::tempdir().expect("temporary vault");
    let vault =
        Vault::open(fs::canonicalize(root.path()).expect("canonical vault")).expect("open vault");
    (root, vault)
}

#[test]
fn replaces_existing_content_at_the_expected_revision() {
    let (root, vault) = fixture();
    fs::create_dir(root.path().join("บันทึก")).unwrap();
    fs::write(root.path().join("บันทึก/สวัสดี 👋.md"), "รุ่นแรก").unwrap();
    let path = VaultPath::from_portable("บันทึก/สวัสดี 👋.md").unwrap();

    assert!(matches!(
        vault
            .replace_content_file_if_revision(
                &path,
                &FileRevision::from_bytes("รุ่นแรก".as_bytes()),
                "รุ่นใหม่ ✅".as_bytes(),
                1024,
            )
            .unwrap(),
        ReplaceContentOutcome::Replaced(_)
    ));
    assert_eq!(
        fs::read(root.path().join(path.as_path())).unwrap(),
        "รุ่นใหม่ ✅".as_bytes()
    );
}

#[test]
fn stale_hash_or_length_never_writes() {
    for expected in [
        FileRevision::from_bytes(b"same"),
        FileRevision::from_bytes(b"different length"),
    ] {
        let (root, vault) = fixture();
        fs::write(root.path().join("note.md"), b"live").unwrap();
        let path = VaultPath::from_portable("note.md").unwrap();

        assert!(matches!(
            vault.replace_content_file_if_revision(&path, &expected, b"replacement", 1024),
            Err(CoreError::StaleRevision { .. })
        ));
        assert_eq!(fs::read(root.path().join("note.md")).unwrap(), b"live");
    }
}

#[test]
fn rejects_hardlinks_directories_and_missing_files() {
    let (root, vault) = fixture();
    fs::write(root.path().join("linked.md"), b"old").unwrap();
    fs::hard_link(root.path().join("linked.md"), root.path().join("alias.md")).unwrap();
    let linked = VaultPath::from_portable("linked.md").unwrap();
    assert!(matches!(
        vault.replace_content_file_if_revision(
            &linked,
            &FileRevision::from_bytes(b"old"),
            b"new",
            1024,
        ),
        Err(CoreError::InvalidMove { .. })
    ));
    assert_eq!(fs::read(root.path().join("alias.md")).unwrap(), b"old");

    fs::create_dir(root.path().join("directory.md")).unwrap();
    for name in ["directory.md", "missing.md"] {
        let path = VaultPath::from_portable(name).unwrap();
        assert!(vault
            .replace_content_file_if_revision(
                &path,
                &FileRevision::from_bytes(b"old"),
                b"new",
                1024,
            )
            .is_err());
    }
}

#[cfg(unix)]
#[test]
fn rejects_a_final_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let (root, vault) = fixture();
    fs::write(root.path().join("target.md"), b"old").unwrap();
    symlink("target.md", root.path().join("link.md")).unwrap();
    let path = VaultPath::from_portable("link.md").unwrap();

    assert!(matches!(
        vault.replace_content_file_if_revision(
            &path,
            &FileRevision::from_bytes(b"old"),
            b"new",
            1024,
        ),
        Err(CoreError::SymlinkRejected(_))
    ));
    assert_eq!(fs::read(root.path().join("target.md")).unwrap(), b"old");
}

#[test]
fn rejects_internal_paths_and_both_size_limits() {
    let (root, vault) = fixture();
    for name in [".obsidian/config.json", ".trash/item"] {
        let path = VaultPath::from_portable(name).unwrap();
        assert!(matches!(
            vault.replace_content_file_if_revision(
                &path,
                &FileRevision::from_bytes(b"old"),
                b"new",
                1024,
            ),
            Err(CoreError::InvalidMove { .. })
        ));
    }

    fs::write(root.path().join("note.md"), b"old").unwrap();
    let path = VaultPath::from_portable("note.md").unwrap();
    for (expected, replacement) in [
        (FileRevision::from_bytes(b"old"), b"large".as_slice()),
        (FileRevision::from_bytes(b"large"), b"new".as_slice()),
    ] {
        assert!(matches!(
            vault.replace_content_file_if_revision(&path, &expected, replacement, 3),
            Err(CoreError::ResourceLimitExceeded { .. })
        ));
        assert_eq!(fs::read(root.path().join("note.md")).unwrap(), b"old");
    }
}

#[test]
fn separately_opened_vaults_serialize_the_same_expected_revision() {
    let (root, first) = fixture();
    fs::write(root.path().join("note.md"), b"old").unwrap();
    let second = Vault::open(fs::canonicalize(root.path()).unwrap()).unwrap();
    let barrier = Arc::new(Barrier::new(2));

    let handles: Vec<_> = [(first, b"first".as_slice()), (second, b"second".as_slice())]
        .into_iter()
        .map(|(vault, contents)| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let path = VaultPath::from_portable("note.md").unwrap();
                barrier.wait();
                vault.replace_content_file_if_revision(
                    &path,
                    &FileRevision::from_bytes(b"old"),
                    contents,
                    1024,
                )
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(ReplaceContentOutcome::Replaced(_))))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CoreError::StaleRevision { .. })))
            .count(),
        1
    );
    let final_bytes = fs::read(root.path().join("note.md")).unwrap();
    assert!(final_bytes == b"first" || final_bytes == b"second");
}
