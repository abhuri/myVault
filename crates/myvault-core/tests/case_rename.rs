use std::fs;

use myvault_core::{CaseRenameOutcome, CoreError, FileRevision, Vault, VaultPath};

fn fixture() -> (tempfile::TempDir, Vault) {
    let root = tempfile::tempdir().expect("temporary vault");
    let vault =
        Vault::open(fs::canonicalize(root.path()).expect("canonical vault")).expect("open vault");
    (root, vault)
}

fn paths() -> (VaultPath, VaultPath, VaultPath) {
    (
        VaultPath::from_portable("Note.md").unwrap(),
        VaultPath::from_portable("note.md").unwrap(),
        VaultPath::from_portable(".mvcr-0123456789abcdef0123456789abcdef.tmp").unwrap(),
    )
}

#[test]
fn fresh_case_rename_uses_two_no_replace_moves() {
    let (root, vault) = fixture();
    let (source, destination, temporary) = paths();
    fs::write(root.path().join(source.as_path()), b"note").unwrap();

    assert!(matches!(
        vault
            .case_rename_content_file_if_revision(
                &source,
                &destination,
                &temporary,
                &FileRevision::from_bytes(b"note"),
            )
            .unwrap(),
        CaseRenameOutcome::Renamed(_)
    ));
    let names: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(!names
        .iter()
        .any(|name| name == source.as_path().file_name().unwrap()));
    assert!(!names
        .iter()
        .any(|name| name == temporary.as_path().file_name().unwrap()));
    assert!(names
        .iter()
        .any(|name| name == destination.as_path().file_name().unwrap()));
    assert_eq!(
        fs::read(root.path().join(destination.as_path())).unwrap(),
        b"note"
    );
}

#[test]
fn retained_case_rename_resumes_temporary_and_confirms_destination() {
    let (root, vault) = fixture();
    let (source, destination, temporary) = paths();
    let revision = FileRevision::from_bytes(b"note");
    fs::write(root.path().join(temporary.as_path()), b"note").unwrap();

    assert!(matches!(
        vault
            .resume_case_rename_content_file_if_revision(
                &source,
                &destination,
                &temporary,
                &revision,
            )
            .unwrap(),
        CaseRenameOutcome::ResumedFromTemporary(_)
    ));
    assert!(matches!(
        vault
            .resume_case_rename_content_file_if_revision(
                &source,
                &destination,
                &temporary,
                &revision,
            )
            .unwrap(),
        CaseRenameOutcome::AlreadyRenamed(_)
    ));
}

#[test]
fn retained_source_only_is_rejected_as_aba() {
    let (root, vault) = fixture();
    let (source, destination, temporary) = paths();
    fs::write(root.path().join(source.as_path()), b"note").unwrap();

    assert!(matches!(
        vault.resume_case_rename_content_file_if_revision(
            &source,
            &destination,
            &temporary,
            &FileRevision::from_bytes(b"note"),
        ),
        Err(CoreError::InvalidMove {
            reason: "source-only topology is ambiguous during retained case rename recovery",
            ..
        })
    ));
    assert_eq!(
        fs::read(root.path().join(source.as_path())).unwrap(),
        b"note"
    );
}

#[test]
fn fresh_temporary_only_and_absent_topologies_fail_closed() {
    let (root, vault) = fixture();
    let (source, destination, temporary) = paths();
    let revision = FileRevision::from_bytes(b"note");
    fs::write(root.path().join(temporary.as_path()), b"note").unwrap();
    assert!(matches!(
        vault.case_rename_content_file_if_revision(&source, &destination, &temporary, &revision,),
        Err(CoreError::InvalidMove {
            reason: "temporary-only topology requires retained case rename recovery",
            ..
        })
    ));
    fs::remove_file(root.path().join(temporary.as_path())).unwrap();
    assert!(matches!(
        vault.case_rename_content_file_if_revision(&source, &destination, &temporary, &revision,),
        Err(CoreError::InvalidMove { .. })
    ));
}

#[test]
fn rejects_stale_nonregular_and_hardlinked_entries() {
    let (root, vault) = fixture();
    let (source, destination, temporary) = paths();
    fs::write(root.path().join(source.as_path()), b"changed").unwrap();
    assert!(matches!(
        vault.case_rename_content_file_if_revision(
            &source,
            &destination,
            &temporary,
            &FileRevision::from_bytes(b"expected"),
        ),
        Err(CoreError::StaleRevision { .. })
    ));

    fs::remove_file(root.path().join(source.as_path())).unwrap();
    fs::create_dir(root.path().join(source.as_path())).unwrap();
    assert!(vault
        .case_rename_content_file_if_revision(
            &source,
            &destination,
            &temporary,
            &FileRevision::from_bytes(b""),
        )
        .is_err());

    #[cfg(unix)]
    {
        fs::remove_dir(root.path().join(source.as_path())).unwrap();
        fs::write(root.path().join(source.as_path()), b"note").unwrap();
        fs::hard_link(
            root.path().join(source.as_path()),
            root.path().join("alias.md"),
        )
        .unwrap();
        assert!(vault
            .case_rename_content_file_if_revision(
                &source,
                &destination,
                &temporary,
                &FileRevision::from_bytes(b"note"),
            )
            .is_err());
    }
}

#[test]
fn supports_unicode_normalization_case_rename() {
    let (root, vault) = fixture();
    let source = VaultPath::from_portable("Cafe\u{301}.md").unwrap();
    let destination = VaultPath::from_portable("CAFÉ.md").unwrap();
    let temporary = VaultPath::from_portable(".mvcr-fedcba9876543210fedcba9876543210.tmp").unwrap();
    fs::write(root.path().join(source.as_path()), b"unicode").unwrap();

    assert!(matches!(
        vault
            .case_rename_content_file_if_revision(
                &source,
                &destination,
                &temporary,
                &FileRevision::from_bytes(b"unicode"),
            )
            .unwrap(),
        CaseRenameOutcome::Renamed(_)
    ));
}

#[test]
fn rejects_invalid_parent_and_collision_relationships() {
    let (_root, vault) = fixture();
    let source = VaultPath::from_portable("one/Note.md").unwrap();
    let destination = VaultPath::from_portable("two/note.md").unwrap();
    let temporary = VaultPath::from_portable("one/temp.md").unwrap();
    assert!(matches!(
        vault.case_rename_content_file_if_revision(
            &source,
            &destination,
            &temporary,
            &FileRevision::from_bytes(b"note"),
        ),
        Err(CoreError::InvalidMove { .. })
    ));

    let destination = VaultPath::from_portable("one/other.md").unwrap();
    assert!(matches!(
        vault.case_rename_content_file_if_revision(
            &source,
            &destination,
            &temporary,
            &FileRevision::from_bytes(b"note"),
        ),
        Err(CoreError::InvalidMove { .. })
    ));
}

#[test]
fn single_link_content_revision_rejects_internal_paths() {
    let (_root, vault) = fixture();
    let internal = VaultPath::from_portable(".obsidian/config.json").unwrap();

    assert!(matches!(
        vault.single_link_content_revision(&internal, 1024),
        Err(CoreError::InvalidMove { .. })
    ));
}

#[cfg(unix)]
#[test]
fn exact_enumeration_rejects_a_third_portable_alias() {
    let (root, vault) = fixture();
    let (source, destination, temporary) = paths();
    fs::write(root.path().join("NOTE.md"), b"note").unwrap();
    assert!(matches!(
        vault.case_rename_content_file_if_revision(
            &source,
            &destination,
            &temporary,
            &FileRevision::from_bytes(b"note"),
        ),
        Err(CoreError::PortablePathCollision { .. })
    ));
}
