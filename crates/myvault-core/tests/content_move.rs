use std::fs;

use myvault_core::{CoreError, FileRevision, MoveContentOutcome, Vault, VaultPath};

fn fixture() -> (tempfile::TempDir, Vault) {
    let root = tempfile::tempdir().expect("temporary vault");
    let vault =
        Vault::open(fs::canonicalize(root.path()).expect("canonical vault")).expect("open vault");
    (root, vault)
}

#[test]
fn content_move_is_atomic_and_idempotent_across_two_retries() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("from.md").unwrap();
    let destination = VaultPath::from_portable("to.md").unwrap();
    fs::write(root.path().join(source.as_path()), b"note").unwrap();
    let expected = FileRevision::from_bytes(b"note");

    assert!(matches!(
        vault
            .move_content_file_if_revision(&source, &destination, &expected)
            .unwrap(),
        MoveContentOutcome::Moved(_)
    ));
    for _ in 0..2 {
        assert!(matches!(
            vault
                .move_content_file_if_revision(&source, &destination, &expected)
                .unwrap(),
            MoveContentOutcome::AlreadyMoved(_)
        ));
    }
    assert_eq!(fs::read(root.path().join("to.md")).unwrap(), b"note");
}

#[test]
fn retained_content_move_recovery_fails_closed_for_source_only_topology() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("from.md").unwrap();
    let destination = VaultPath::from_portable("to.md").unwrap();
    fs::write(root.path().join(source.as_path()), b"note").unwrap();

    assert!(matches!(
        vault.resume_content_file_move_if_revision(
            &source,
            &destination,
            &FileRevision::from_bytes(b"note")
        ),
        Err(CoreError::InvalidMove {
            reason: "source-only topology is ambiguous during retained move recovery",
            ..
        })
    ));
    assert_eq!(
        fs::read(root.path().join(source.as_path())).unwrap(),
        b"note"
    );
    assert!(!root.path().join(destination.as_path()).exists());
}

#[test]
fn retained_content_move_recovery_accepts_verified_destination_only_topology() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("from.md").unwrap();
    let destination = VaultPath::from_portable("to.md").unwrap();
    fs::write(root.path().join(destination.as_path()), b"note").unwrap();

    assert!(matches!(
        vault
            .resume_content_file_move_if_revision(
                &source,
                &destination,
                &FileRevision::from_bytes(b"note")
            )
            .unwrap(),
        MoveContentOutcome::AlreadyMoved(_)
    ));
}

#[test]
fn retained_content_move_recovery_preserves_topology_and_revision_checks() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("from.md").unwrap();
    let destination = VaultPath::from_portable("to.md").unwrap();
    fs::write(root.path().join(destination.as_path()), b"other").unwrap();
    assert!(matches!(
        vault.resume_content_file_move_if_revision(
            &source,
            &destination,
            &FileRevision::from_bytes(b"expected")
        ),
        Err(CoreError::StaleRevision { .. })
    ));

    fs::write(root.path().join(source.as_path()), b"other").unwrap();
    assert!(matches!(
        vault.resume_content_file_move_if_revision(
            &source,
            &destination,
            &FileRevision::from_bytes(b"other")
        ),
        Err(CoreError::AlreadyExists(_))
    ));

    fs::remove_file(root.path().join(source.as_path())).unwrap();
    fs::remove_file(root.path().join(destination.as_path())).unwrap();
    assert!(matches!(
        vault.resume_content_file_move_if_revision(
            &source,
            &destination,
            &FileRevision::from_bytes(b"other")
        ),
        Err(CoreError::InvalidMove { .. })
    ));

    let case_alias = VaultPath::from_portable("FROM.md").unwrap();
    assert!(matches!(
        vault.resume_content_file_move_if_revision(
            &source,
            &case_alias,
            &FileRevision::from_bytes(b"other")
        ),
        Err(CoreError::InvalidMove { .. })
    ));
}

#[test]
fn concurrent_content_move_callers_serialize_to_moved_and_already_moved() {
    use std::sync::{Arc, Barrier};

    let (root, _vault) = fixture();
    fs::create_dir(root.path().join("from")).unwrap();
    fs::create_dir(root.path().join("to")).unwrap();
    fs::write(root.path().join("from/note.md"), b"note").unwrap();
    let canonical = fs::canonicalize(root.path()).unwrap();
    let first = Vault::open(&canonical).unwrap();
    let second = Vault::open(&canonical).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let run = |vault: Vault, barrier: Arc<Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            vault.move_content_file_if_revision(
                &VaultPath::from_portable("from/note.md").unwrap(),
                &VaultPath::from_portable("to/note.md").unwrap(),
                &FileRevision::from_bytes(b"note"),
            )
        })
    };
    let one = run(first, Arc::clone(&barrier));
    let two = run(second, barrier);
    let outcomes = [one.join().unwrap().unwrap(), two.join().unwrap().unwrap()];

    assert!(outcomes
        .iter()
        .any(|outcome| matches!(outcome, MoveContentOutcome::Moved(_))));
    assert!(outcomes
        .iter()
        .any(|outcome| matches!(outcome, MoveContentOutcome::AlreadyMoved(_))));
}

#[test]
fn content_move_rejects_stale_both_neither_and_collision_aliases() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("from.md").unwrap();
    let destination = VaultPath::from_portable("to.md").unwrap();
    fs::write(root.path().join(source.as_path()), b"changed").unwrap();
    assert!(matches!(
        vault.move_content_file_if_revision(
            &source,
            &destination,
            &FileRevision::from_bytes(b"expected")
        ),
        Err(CoreError::StaleRevision { .. })
    ));

    fs::write(root.path().join(destination.as_path()), b"changed").unwrap();
    assert!(matches!(
        vault.move_content_file_if_revision(
            &source,
            &destination,
            &FileRevision::from_bytes(b"changed")
        ),
        Err(CoreError::AlreadyExists(_))
    ));
    fs::remove_file(root.path().join(source.as_path())).unwrap();
    fs::remove_file(root.path().join(destination.as_path())).unwrap();
    assert!(matches!(
        vault.move_content_file_if_revision(
            &source,
            &destination,
            &FileRevision::from_bytes(b"changed")
        ),
        Err(CoreError::InvalidMove { .. })
    ));

    let case_alias = VaultPath::from_portable("FROM.md").unwrap();
    assert!(matches!(
        vault.move_content_file_if_revision(
            &source,
            &case_alias,
            &FileRevision::from_bytes(b"changed")
        ),
        Err(CoreError::InvalidMove { .. })
    ));
}

#[test]
fn content_move_rejects_destination_mismatch_and_nonregular_source() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("from.md").unwrap();
    let destination = VaultPath::from_portable("to.md").unwrap();
    fs::write(root.path().join(destination.as_path()), b"other").unwrap();
    assert!(matches!(
        vault.move_content_file_if_revision(
            &source,
            &destination,
            &FileRevision::from_bytes(b"expected")
        ),
        Err(CoreError::StaleRevision { .. })
    ));

    fs::remove_file(root.path().join(destination.as_path())).unwrap();
    fs::create_dir(root.path().join(source.as_path())).unwrap();
    assert!(vault
        .move_content_file_if_revision(&source, &destination, &FileRevision::from_bytes(b""))
        .is_err());
}

#[cfg(unix)]
#[test]
fn content_move_rejects_symlink_and_hardlinked_source() {
    use std::os::unix::fs::symlink;

    let (root, vault) = fixture();
    let source = VaultPath::from_portable("from.md").unwrap();
    let destination = VaultPath::from_portable("to.md").unwrap();
    fs::write(root.path().join("target.md"), b"note").unwrap();
    symlink(
        root.path().join("target.md"),
        root.path().join(source.as_path()),
    )
    .unwrap();
    assert!(vault
        .move_content_file_if_revision(&source, &destination, &FileRevision::from_bytes(b"note"))
        .is_err());

    fs::remove_file(root.path().join(source.as_path())).unwrap();
    fs::write(root.path().join(source.as_path()), b"note").unwrap();
    fs::hard_link(
        root.path().join(source.as_path()),
        root.path().join("alias.md"),
    )
    .unwrap();
    assert!(vault
        .move_content_file_if_revision(&source, &destination, &FileRevision::from_bytes(b"note"))
        .is_err());

    let (root, vault) = fixture();
    let source = VaultPath::from_portable("from.md").unwrap();
    let destination = VaultPath::from_portable("to.md").unwrap();
    fs::write(root.path().join("target.md"), b"note").unwrap();
    symlink(
        root.path().join("target.md"),
        root.path().join(destination.as_path()),
    )
    .unwrap();
    assert!(vault
        .move_content_file_if_revision(&source, &destination, &FileRevision::from_bytes(b"note"))
        .is_err());
    fs::remove_file(root.path().join(destination.as_path())).unwrap();
    fs::write(root.path().join(destination.as_path()), b"note").unwrap();
    fs::hard_link(
        root.path().join(destination.as_path()),
        root.path().join("destination-alias.md"),
    )
    .unwrap();
    assert!(vault
        .move_content_file_if_revision(&source, &destination, &FileRevision::from_bytes(b"note"))
        .is_err());
}
