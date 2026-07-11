use myvault_recovery::{
    decide_recovery, Error, FileRevision, RecoveryDecision, RecoveryJournal, RecoveryTopology,
    RenameMoveIntent,
};
#[cfg(unix)]
use myvault_recovery::{CompleteOutcome, PublishOutcome, MAX_PAGE_SIZE};
use std::fs;
#[cfg(unix)]
use std::io::Write;
use tempfile::TempDir;
#[cfg(unix)]
use uuid::Uuid;

fn revision(text: &str) -> FileRevision {
    FileRevision::from_bytes(text.as_bytes())
}

fn intent() -> RenameMoveIntent {
    RenameMoveIntent::new(
        "บันทึก/ต้นทาง.md",
        "คลัง/ปลายทาง.md",
        revision("note"),
        Some(".tmp/ย้าย.md".into()),
    )
    .unwrap()
}

fn roots() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temporary = TempDir::new().unwrap();
    let base = temporary.path().canonicalize().unwrap();
    let app = base.join("app");
    let vault = base.join("vault");
    fs::create_dir(&app).unwrap();
    fs::create_dir(&vault).unwrap();
    make_private(&app);
    (temporary, app, vault)
}

#[cfg(unix)]
fn make_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(not(unix))]
fn make_private(_path: &std::path::Path) {}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn classifies_every_recovery_topology() {
    let intent = intent();
    let expected = intent.expected.clone();
    let other = revision("external");
    let cases = [
        (
            RecoveryTopology {
                from: Some(expected.clone()),
                to: None,
                temp: None,
            },
            RecoveryDecision::NotStarted,
        ),
        (
            RecoveryTopology {
                from: None,
                to: None,
                temp: Some(expected.clone()),
            },
            RecoveryDecision::InProgressAtTemp,
        ),
        (
            RecoveryTopology {
                from: None,
                to: Some(expected.clone()),
                temp: None,
            },
            RecoveryDecision::Committed,
        ),
        (
            RecoveryTopology {
                from: Some(expected.clone()),
                to: Some(other.clone()),
                temp: None,
            },
            RecoveryDecision::DestinationCollision,
        ),
        (
            RecoveryTopology {
                from: Some(expected.clone()),
                to: Some(expected.clone()),
                temp: None,
            },
            RecoveryDecision::DuplicateManual,
        ),
        (RecoveryTopology::default(), RecoveryDecision::DataLoss),
        (
            RecoveryTopology {
                from: Some(other.clone()),
                to: None,
                temp: Some(expected.clone()),
            },
            RecoveryDecision::ExternalMutation,
        ),
    ];
    for (topology, expected_decision) in cases {
        assert_eq!(decide_recovery(&intent, &topology), expected_decision);
    }

    for from in [None, Some(expected.clone()), Some(other.clone())] {
        for to in [None, Some(expected.clone()), Some(other.clone())] {
            for temp in [None, Some(expected.clone()), Some(other.clone())] {
                let topology = RecoveryTopology {
                    from: from.clone(),
                    to: to.clone(),
                    temp: temp.clone(),
                };
                let exhaustive_expected = match (&from, &to, &temp) {
                    (Some(value), None, None) if value == &expected => RecoveryDecision::NotStarted,
                    (None, None, Some(value)) if value == &expected => {
                        RecoveryDecision::InProgressAtTemp
                    }
                    (None, Some(value), None) if value == &expected => RecoveryDecision::Committed,
                    (Some(source), Some(destination), None)
                        if source == &expected && destination == &expected =>
                    {
                        RecoveryDecision::DuplicateManual
                    }
                    (Some(source), Some(destination), None)
                        if source == &expected && destination != &expected =>
                    {
                        RecoveryDecision::DestinationCollision
                    }
                    (None, None, None) => RecoveryDecision::DataLoss,
                    _ => RecoveryDecision::ExternalMutation,
                };
                assert_eq!(decide_recovery(&intent, &topology), exhaustive_expected);
            }
        }
    }
}

#[test]
#[cfg(unix)]
fn round_trips_thai_paths_and_lists_entries() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let intent = intent();
    journal.publish(&intent).unwrap();
    assert_eq!(journal.read(intent.operation_id).unwrap(), intent);
    assert_eq!(journal.list().unwrap(), vec![intent]);
}

#[test]
#[cfg(unix)]
fn malformed_json_is_rejected() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let id = Uuid::new_v4();
    write_private(
        &app.join("operation-journal").join(format!("{id}.json")),
        b"{",
    );
    assert!(matches!(journal.read(id), Err(Error::Json(_))));
}

#[test]
#[cfg(unix)]
fn oversized_json_is_rejected_before_parsing() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let id = Uuid::new_v4();
    let mut file =
        fs::File::create(app.join("operation-journal").join(format!("{id}.json"))).unwrap();
    file.write_all(&vec![b'x'; 65 * 1024]).unwrap();
    assert!(matches!(journal.read(id), Err(Error::EntryTooLarge)));
}

#[test]
#[cfg(unix)]
fn crash_temporary_file_is_ignored() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    fs::write(
        app.join("operation-journal/.abandoned.json.tmp"),
        b"partial",
    )
    .unwrap();
    assert!(journal.list().unwrap().is_empty());
}

#[test]
#[cfg(unix)]
fn uuid_collision_preserves_existing_committed_entry() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let original = intent();
    journal.publish(&original).unwrap();
    let original_bytes = fs::read(
        app.join("operation-journal")
            .join(format!("{}.json", original.operation_id)),
    )
    .unwrap();
    assert_eq!(
        journal.publish(&original).unwrap(),
        PublishOutcome::AlreadyPublished
    );
    assert_eq!(
        fs::read(
            app.join("operation-journal")
                .join(format!("{}.json", original.operation_id))
        )
        .unwrap(),
        original_bytes
    );
}

#[test]
#[cfg(unix)]
fn reconciles_crash_before_and_after_hard_link() {
    let (_temporary, app, vault) = roots();
    let journal_dir = app.join("operation-journal");
    let journal = RecoveryJournal::open(&app, &vault).unwrap();

    let before_link = intent();
    let before_bytes = serde_json::to_vec(&before_link).unwrap();
    let before_temp = journal_dir.join(format!(".{}.json.tmp", before_link.operation_id));
    write_private(&before_temp, &before_bytes);
    assert_eq!(
        journal.publish(&before_link).unwrap(),
        PublishOutcome::ReconciledAfterTempWrite
    );
    assert!(!before_temp.exists());

    let after_link = intent();
    let after_bytes = serde_json::to_vec(&after_link).unwrap();
    let after_temp = journal_dir.join(format!(".{}.json.tmp", after_link.operation_id));
    let after_final = journal_dir.join(format!("{}.json", after_link.operation_id));
    write_private(&after_temp, &after_bytes);
    fs::hard_link(&after_temp, &after_final).unwrap();
    assert_eq!(
        journal.publish(&after_link).unwrap(),
        PublishOutcome::AlreadyPublishedAndCleanedTemp
    );
    assert!(!after_temp.exists());
    assert_eq!(journal.read(after_link.operation_id).unwrap(), after_link);
}

#[test]
#[cfg(unix)]
fn publish_fails_closed_on_collisions_and_unexpected_temp() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let expected = intent();
    let mut collision = expected.clone();
    collision.to = "other/destination.md".into();
    let final_path = app
        .join("operation-journal")
        .join(format!("{}.json", expected.operation_id));
    write_private(&final_path, &serde_json::to_vec(&collision).unwrap());
    assert!(matches!(
        journal.publish(&expected),
        Err(Error::JournalCollision)
    ));

    let temp_path = app
        .join("operation-journal")
        .join(format!(".{}.json.tmp", expected.operation_id));
    write_private(&temp_path, &serde_json::to_vec(&expected).unwrap());
    assert!(matches!(
        journal.publish(&expected),
        Err(Error::JournalCollision)
    ));
    assert!(!temp_path.exists());

    write_private(&temp_path, b"external");
    assert!(matches!(
        journal.publish(&expected),
        Err(Error::ExternalMutation)
    ));
    assert_eq!(fs::read(temp_path).unwrap(), b"external");
}

#[cfg(unix)]
#[test]
fn committed_file_privacy_is_verified_not_repaired() {
    use std::os::unix::fs::PermissionsExt;
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let expected = intent();
    journal.publish(&expected).unwrap();
    let final_path = app
        .join("operation-journal")
        .join(format!("{}.json", expected.operation_id));
    fs::set_permissions(&final_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        journal.read(expected.operation_id),
        Err(Error::ExternalMutation)
    ));
    assert_eq!(
        fs::metadata(&final_path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
#[cfg(unix)]
fn complete_requires_exact_expected_intent_and_syncs_removal() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let expected = intent();
    journal.publish(&expected).unwrap();
    let mut wrong = expected.clone();
    wrong.to = "other/destination.md".into();
    assert!(matches!(
        journal.complete(expected.operation_id, &wrong),
        Err(Error::IntentMismatch)
    ));
    assert_eq!(
        journal.complete(expected.operation_id, &expected).unwrap(),
        CompleteOutcome::Completed
    );
    assert_eq!(
        journal.complete(expected.operation_id, &expected).unwrap(),
        CompleteOutcome::AlreadyCompleted
    );
}

#[test]
#[cfg(unix)]
fn complete_rejects_a_crash_temp_instead_of_authorizing_cleanup() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let expected = intent();
    journal.publish(&expected).unwrap();
    let temp_path = app
        .join("operation-journal")
        .join(format!(".{}.json.tmp", expected.operation_id));
    write_private(&temp_path, &serde_json::to_vec(&expected).unwrap());
    assert!(matches!(
        journal.complete(expected.operation_id, &expected),
        Err(Error::ExternalMutation)
    ));
}

#[test]
#[cfg(unix)]
fn pagination_is_bounded_and_junk_does_not_consume_committed_limit() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    for _ in 0..(MAX_PAGE_SIZE + 2) {
        journal.publish(&intent()).unwrap();
    }
    for index in 0..4_200 {
        write_private(
            &app.join("operation-journal")
                .join(format!("junk-{index}.tmp")),
            b"junk",
        );
    }
    write_private(
        &app.join("operation-journal")
            .join("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA.json"),
        b"junk",
    );
    let first = journal.list_page(None, MAX_PAGE_SIZE).unwrap();
    assert_eq!(first.entries.len(), MAX_PAGE_SIZE);
    assert!(first.next_after.is_some());
    let second = journal.list_page(first.next_after, MAX_PAGE_SIZE).unwrap();
    assert_eq!(second.entries.len(), 2);
    assert!(second.next_after.is_none());
    assert_eq!(journal.list().unwrap().len(), MAX_PAGE_SIZE + 2);
    assert!(matches!(
        journal.list_page(None, 0),
        Err(Error::InvalidPageSize)
    ));
}

#[test]
fn paths_are_canonical_and_case_rename_requires_explicit_contract() {
    let canonical = RenameMoveIntent::new(
        "folder//source.md",
        "other/./target.md",
        revision("note"),
        None,
    )
    .unwrap();
    assert_eq!(canonical.from, "folder/source.md");
    assert_eq!(canonical.to, "other/target.md");
    assert!(matches!(
        RenameMoveIntent::new("Note.md", "note.md", revision("note"), None),
        Err(Error::CaseRenameContractRequired)
    ));
    assert!(matches!(
        RenameMoveIntent::new("same.md", "same.md", revision("note"), None),
        Err(Error::IdenticalPaths)
    ));
    let case = RenameMoveIntent::new_case_rename(
        "Note.md",
        "note.md",
        revision("note"),
        ".rename-stage/unique.md",
    )
    .unwrap();
    assert!(case.case_rename);
}

#[cfg(unix)]
#[test]
fn rejects_symlink_components_and_sets_private_permissions() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let (temporary, app, vault) = roots();
    let actual = temporary.path().join("actual");
    fs::create_dir(&actual).unwrap();
    let linked = temporary.path().join("linked");
    symlink(&actual, &linked).unwrap();
    assert!(matches!(
        RecoveryJournal::open(&linked, &vault),
        Err(Error::InvalidRoot(_))
    ));

    let poisoned_app = temporary.path().canonicalize().unwrap().join("poisoned");
    fs::create_dir(&poisoned_app).unwrap();
    make_private(&poisoned_app);
    symlink(&actual, poisoned_app.join("operation-journal")).unwrap();
    assert!(matches!(
        RecoveryJournal::open(&poisoned_app, &vault),
        Err(Error::InvalidRoot(_))
    ));

    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let intent = intent();
    journal.publish(&intent).unwrap();
    let directory_mode = fs::metadata(app.join("operation-journal"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let file_mode = fs::metadata(
        app.join("operation-journal")
            .join(format!("{}.json", intent.operation_id)),
    )
    .unwrap()
    .permissions()
    .mode()
        & 0o777;
    assert_eq!(directory_mode, 0o700);
    assert_eq!(file_mode, 0o600);

    let malicious_id = Uuid::new_v4();
    let target = app.join("target.json");
    fs::write(&target, b"{}").unwrap();
    symlink(
        &target,
        app.join("operation-journal")
            .join(format!("{malicious_id}.json")),
    )
    .unwrap();
    assert!(journal.read(malicious_id).is_err());
}

#[cfg(unix)]
#[test]
fn rejects_broad_app_root_without_changing_it() {
    use std::os::unix::fs::PermissionsExt;
    let (_temporary, app, vault) = roots();
    fs::set_permissions(&app, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        RecoveryJournal::open(&app, &vault),
        Err(Error::InvalidRoot(_))
    ));
    assert!(!app.join("operation-journal").exists());
    assert_eq!(
        fs::metadata(&app).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
#[cfg(unix)]
fn rejects_overlapping_roots() {
    let temporary = TempDir::new().unwrap();
    let base = temporary.path().canonicalize().unwrap();
    let app = base.join("app");
    let vault = app.join("vault");
    fs::create_dir(&app).unwrap();
    fs::create_dir(&vault).unwrap();
    make_private(&app);
    assert!(matches!(
        RecoveryJournal::open(&app, &vault),
        Err(Error::InvalidRoot(_))
    ));
}

#[cfg(windows)]
#[test]
fn windows_fails_closed_until_acl_privacy_validation_exists() {
    let (_temporary, app, vault) = roots();
    assert!(matches!(
        RecoveryJournal::open(&app, &vault),
        Err(Error::PrivacyValidationRequired)
    ));
}
