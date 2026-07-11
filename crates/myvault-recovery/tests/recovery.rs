use myvault_recovery::{
    decide_recovery, Error, FileRevision, RecoveryDecision, RecoveryJournal, RecoveryOperationKind,
    RecoveryTopology, RenameMoveIntent,
};
#[cfg(unix)]
use myvault_recovery::{CompleteOutcome, PublishOutcome, MAX_PAGE_SIZE};
use std::fs;
#[cfg(unix)]
use std::io::Write;
use tempfile::TempDir;
use uuid::Uuid;

fn revision(text: &str) -> FileRevision {
    FileRevision::from_bytes(text.as_bytes())
}

fn intent() -> RenameMoveIntent {
    RenameMoveIntent::new(
        Uuid::new_v4(),
        "บันทึก/ต้นทาง.md",
        "คลัง/ปลายทาง.md",
        revision("note"),
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn add_extended_acl(path: &std::path::Path, mode: u32) {
    use exacl::{AclEntry, Perm};
    use std::os::unix::fs::PermissionsExt;

    let mut entries = exacl::getfacl(path, None).unwrap();
    entries.push(AclEntry::allow_user(
        &rustix::process::geteuid().as_raw().to_string(),
        Perm::READ,
        None,
    ));
    exacl::setfacl(&[path], &entries, None).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        mode
    );
    assert!(!exacl::getfacl(path, None).unwrap().is_empty());
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
    assert_eq!(
        journal.list_page(None, MAX_PAGE_SIZE).unwrap().entries,
        vec![intent]
    );
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
    assert!(journal
        .list_page(None, MAX_PAGE_SIZE)
        .unwrap()
        .entries
        .is_empty());
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
fn stale_partial_temps_are_ignored_and_preserved() {
    let (_temporary, app, vault) = roots();
    let journal_dir = app.join("operation-journal");
    let journal = RecoveryJournal::open(&app, &vault).unwrap();

    for (index, partial) in [b"".as_slice(), b"{\"version\":".as_slice()]
        .into_iter()
        .enumerate()
    {
        let expected = intent();
        let temp = journal_dir.join(format!(".publish-stale-{index}.tmp"));
        write_private(&temp, partial);
        assert_eq!(
            journal.publish(&expected).unwrap(),
            PublishOutcome::Published
        );
        assert_eq!(fs::read(&temp).unwrap(), partial);
        assert_eq!(journal.read(expected.operation_id).unwrap(), expected);
    }
}

#[test]
#[cfg(unix)]
fn publish_never_unlinks_symlink_hardlink_or_insecure_stale_temps() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let (_temporary, app, vault) = roots();
    let journal_dir = app.join("operation-journal");
    let journal = RecoveryJournal::open(&app, &vault).unwrap();

    let symlink_temp = journal_dir.join(".publish-swapped-symlink.tmp");
    let symlink_target = app.join("symlink-target");
    write_private(&symlink_target, b"partial");
    symlink(&symlink_target, &symlink_temp).unwrap();
    journal.publish(&intent()).unwrap();
    assert!(fs::symlink_metadata(&symlink_temp)
        .unwrap()
        .file_type()
        .is_symlink());

    let hardlink_temp = journal_dir.join(".publish-swapped-hardlink.tmp");
    let hardlink_target = app.join("hardlink-target");
    write_private(&hardlink_target, b"partial");
    fs::hard_link(&hardlink_target, &hardlink_temp).unwrap();
    journal.publish(&intent()).unwrap();
    assert!(hardlink_temp.exists());
    assert!(hardlink_target.exists());

    let insecure_temp = journal_dir.join(".publish-insecure.tmp");
    write_private(&insecure_temp, b"partial");
    fs::set_permissions(&insecure_temp, fs::Permissions::from_mode(0o644)).unwrap();
    journal.publish(&intent()).unwrap();
    assert!(insecure_temp.exists());
}

#[test]
#[cfg(unix)]
fn publish_fails_closed_on_collision_without_unlinking_any_evidence() {
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

    let temp_path = app.join("operation-journal/.publish-unrelated.tmp");
    write_private(&temp_path, b"unrelated");
    assert!(matches!(
        journal.publish(&expected),
        Err(Error::JournalCollision)
    ));
    assert_eq!(fs::read(&temp_path).unwrap(), b"unrelated");
    assert_eq!(
        fs::read(&final_path).unwrap(),
        serde_json::to_vec(&collision).unwrap()
    );
    assert!(fs::read_dir(app.join("operation-journal"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with(".publish-")));
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
fn complete_publishes_tombstone_and_preserves_journal() {
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
    assert_eq!(journal.read(expected.operation_id).unwrap(), expected);
    assert!(journal
        .list_page(None, MAX_PAGE_SIZE)
        .unwrap()
        .entries
        .is_empty());
    assert!(app
        .join("operation-journal/completed")
        .join(format!("{}.json", expected.operation_id))
        .exists());
}

#[test]
#[cfg(unix)]
fn complete_ignores_and_preserves_stale_temp_evidence() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let expected = intent();
    journal.publish(&expected).unwrap();
    let temp_path = app.join("operation-journal/.publish-stale-complete.tmp");
    write_private(&temp_path, b"partial");
    assert_eq!(
        journal.complete(expected.operation_id, &expected).unwrap(),
        CompleteOutcome::Completed
    );
    assert_eq!(fs::read(temp_path).unwrap(), b"partial");
}

#[test]
#[cfg(unix)]
fn completion_collision_preserves_journal_tombstone_and_unrelated_file() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let expected = intent();
    journal.publish(&expected).unwrap();
    let completed = app.join("operation-journal/completed");
    let tombstone = completed.join(format!("{}.json", expected.operation_id));
    write_private(&tombstone, b"{}");
    let unrelated = completed.join("unrelated.keep");
    write_private(&unrelated, b"keep");

    assert!(journal.complete(expected.operation_id, &expected).is_err());
    assert_eq!(fs::read(&tombstone).unwrap(), b"{}");
    assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
    assert_eq!(journal.read(expected.operation_id).unwrap(), expected);
    assert_eq!(
        journal.list_page(None, MAX_PAGE_SIZE).unwrap().entries,
        vec![expected]
    );
}

#[test]
#[cfg(unix)]
fn pagination_is_bounded_and_junk_does_not_consume_committed_limit() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let mut published = Vec::new();
    for _ in 0..(MAX_PAGE_SIZE + 4) {
        let entry = intent();
        journal.publish(&entry).unwrap();
        published.push(entry);
    }
    for entry in &published[..2] {
        journal.complete(entry.operation_id, entry).unwrap();
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
    assert!(matches!(
        journal.list_page(None, 0),
        Err(Error::InvalidPageSize)
    ));
}

#[test]
fn paths_are_canonical_and_case_rename_requires_explicit_contract() {
    let canonical = RenameMoveIntent::new(
        Uuid::new_v4(),
        "folder//source.md",
        "other/./target.md",
        revision("note"),
    )
    .unwrap();
    assert_eq!(canonical.from, "folder/source.md");
    assert_eq!(canonical.to, "other/target.md");
    assert!(matches!(
        RenameMoveIntent::new(Uuid::new_v4(), "Note.md", "note.md", revision("note")),
        Err(Error::CaseRenameContractRequired)
    ));
    assert!(matches!(
        RenameMoveIntent::new(Uuid::new_v4(), "same.md", "same.md", revision("note")),
        Err(Error::IdenticalPaths)
    ));
    let operation_id = Uuid::new_v4();
    let expected_temp = format!(".rename-stage/{operation_id}.tmp");
    let case = RenameMoveIntent::new_case_rename(
        operation_id,
        "Note.md",
        "note.md",
        revision("note"),
        &expected_temp,
    )
    .unwrap();
    assert_eq!(case.kind, RecoveryOperationKind::CaseRename);
    assert_eq!(case.temp.as_deref(), Some(expected_temp.as_str()));
}

#[test]
fn caller_supplied_operation_id_is_stable_and_identifiers_are_validated() {
    let operation_id = Uuid::new_v4();
    let expected = RenameMoveIntent::new(
        operation_id,
        "source.md",
        "destination.md",
        revision("note"),
    )
    .unwrap();
    assert_eq!(expected.operation_id, operation_id);
    assert!(matches!(
        RenameMoveIntent::new(Uuid::nil(), "source.md", "destination.md", revision("note"),),
        Err(Error::InvalidOperationId)
    ));

    let manifest = blake3::hash(b"manifest").to_hex().to_string();
    assert!(matches!(
        RenameMoveIntent::new_trash(
            Uuid::new_v4(),
            Uuid::nil(),
            manifest.clone(),
            "source.md",
            revision("note"),
        ),
        Err(Error::InvalidTrashId)
    ));
    assert!(matches!(
        RenameMoveIntent::new_trash(
            Uuid::new_v4(),
            Uuid::new_v4(),
            manifest.to_uppercase(),
            "source.md",
            revision("note"),
        ),
        Err(Error::InvalidManifestDigest)
    ));
}

#[test]
#[cfg(unix)]
fn every_operation_kind_round_trips_deterministically() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let trash_id = Uuid::new_v4();
    let manifest = blake3::hash(b"manifest").to_hex().to_string();
    let cases = vec![
        RenameMoveIntent::new(
            Uuid::new_v4(),
            "source.md",
            "destination.md",
            revision("note"),
        )
        .unwrap(),
        RenameMoveIntent::new_case_rename(
            Uuid::new_v4(),
            "Note.md",
            "note.md",
            revision("note"),
            ".rename-stage/roundtrip.tmp",
        )
        .unwrap(),
        RenameMoveIntent::new_trash(
            Uuid::new_v4(),
            trash_id,
            manifest.clone(),
            "source.md",
            revision("note"),
        )
        .unwrap(),
        RenameMoveIntent::new_restore(
            Uuid::new_v4(),
            trash_id,
            manifest,
            "restored.md",
            revision("note"),
        )
        .unwrap(),
    ];

    let expected_staging = format!(".trash/v1/staging/{trash_id}/payload");
    let expected_item = format!(".trash/v1/items/{trash_id}/payload");
    assert_eq!(cases[2].temp.as_deref(), Some(expected_staging.as_str()));
    assert_eq!(cases[2].to, expected_item);
    assert_eq!(cases[3].from, expected_item);
    assert!(cases[3].temp.is_none());

    for expected in cases {
        let canonical = serde_json::to_vec(&expected).unwrap();
        assert_eq!(serde_json::to_vec(&expected).unwrap(), canonical);
        assert_eq!(
            journal.publish(&expected).unwrap(),
            PublishOutcome::Published
        );
        assert_eq!(journal.read(expected.operation_id).unwrap(), expected);
        assert_eq!(
            journal.publish(&expected).unwrap(),
            PublishOutcome::AlreadyPublished
        );
    }
}

#[test]
#[cfg(unix)]
fn same_id_with_different_kind_or_payload_is_a_collision() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let operation_id = Uuid::new_v4();
    let normal = RenameMoveIntent::new(
        operation_id,
        "source.md",
        "destination.md",
        revision("note"),
    )
    .unwrap();
    journal.publish(&normal).unwrap();
    let trash = RenameMoveIntent::new_trash(
        operation_id,
        Uuid::new_v4(),
        blake3::hash(b"manifest").to_hex().to_string(),
        "source.md",
        revision("note"),
    )
    .unwrap();
    assert!(matches!(
        journal.publish(&trash),
        Err(Error::JournalCollision)
    ));

    let mut changed_payload = normal.clone();
    changed_payload.expected = revision("different");
    assert!(matches!(
        journal.publish(&changed_payload),
        Err(Error::JournalCollision)
    ));
}

#[test]
fn constructors_reject_protected_endpoint_aliases() {
    assert!(matches!(
        RenameMoveIntent::new(
            Uuid::new_v4(),
            ".ＴＲＡＳＨ/file.md",
            "destination.md",
            revision("note"),
        ),
        Err(Error::InvalidOperationTopology)
    ));
    assert!(matches!(
        RenameMoveIntent::new(
            Uuid::new_v4(),
            "source.md",
            ".Obsidian/plugin.json",
            revision("note"),
        ),
        Err(Error::InvalidOperationTopology)
    ));
    assert!(matches!(
        RenameMoveIntent::new_case_rename(
            Uuid::new_v4(),
            ".trash/Note.md",
            ".trash/note.md",
            revision("note"),
            ".rename-stage/protected-endpoints.tmp",
        ),
        Err(Error::InvalidOperationTopology)
    ));
    assert!(matches!(
        RenameMoveIntent::new_case_rename(
            Uuid::new_v4(),
            "Note.md",
            "note.md",
            revision("note"),
            ".trash/v1/staging/not-allowed/payload",
        ),
        Err(Error::InvalidOperationTopology)
    ));
    assert!(matches!(
        RenameMoveIntent::new_restore(
            Uuid::new_v4(),
            Uuid::new_v4(),
            blake3::hash(b"manifest").to_hex().to_string(),
            ".trash/restored.md",
            revision("note"),
        ),
        Err(Error::InvalidOperationTopology)
    ));
}

#[test]
#[cfg(unix)]
fn decoded_public_structs_reject_cross_kind_endpoint_topologies() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let manifest = blake3::hash(b"manifest").to_hex().to_string();

    let reject = |candidate: RenameMoveIntent| {
        write_private(
            &app.join("operation-journal")
                .join(format!("{}.json", candidate.operation_id)),
            &serde_json::to_vec(&candidate).unwrap(),
        );
        assert!(matches!(
            journal.read(candidate.operation_id),
            Err(Error::InvalidOperationTopology
                | Error::CaseRenameContractRequired
                | Error::InvalidCaseRenameContract)
        ));
    };

    let trash_id = Uuid::new_v4();
    let trash_item = format!(".trash/v1/items/{trash_id}/payload");
    let trash_stage = format!(".trash/v1/staging/{trash_id}/payload");

    let normal = || {
        RenameMoveIntent::new(
            Uuid::new_v4(),
            "source.md",
            "destination.md",
            revision("note"),
        )
        .unwrap()
    };
    let mut candidate = normal();
    candidate.from = trash_item.clone();
    reject(candidate);
    let mut candidate = normal();
    candidate.to = trash_item.clone();
    reject(candidate);
    let mut candidate = normal();
    candidate.temp = Some(trash_stage.clone());
    reject(candidate);

    let case = || {
        RenameMoveIntent::new_case_rename(
            Uuid::new_v4(),
            "Note.md",
            "note.md",
            revision("note"),
            ".rename-stage/mutation.tmp",
        )
        .unwrap()
    };
    let mut candidate = case();
    candidate.from = trash_item.clone();
    reject(candidate);
    let mut candidate = case();
    candidate.to = trash_item.clone();
    reject(candidate);
    let mut candidate = case();
    candidate.temp = Some(trash_stage.clone());
    reject(candidate);

    let trash = || {
        RenameMoveIntent::new_trash(
            Uuid::new_v4(),
            trash_id,
            manifest.clone(),
            "source.md",
            revision("note"),
        )
        .unwrap()
    };
    let mut candidate = trash();
    candidate.from = trash_item.clone();
    reject(candidate);
    let mut candidate = trash();
    candidate.to = trash_stage.clone();
    reject(candidate);
    let mut candidate = trash();
    candidate.temp = Some(trash_item.clone());
    reject(candidate);
    let restore = || {
        RenameMoveIntent::new_restore(
            Uuid::new_v4(),
            trash_id,
            manifest.clone(),
            "restored.md",
            revision("note"),
        )
        .unwrap()
    };
    let mut candidate = restore();
    candidate.from = trash_stage.clone();
    reject(candidate);
    let mut candidate = restore();
    candidate.to = trash_item.clone();
    reject(candidate);
    let mut candidate = restore();
    candidate.temp = Some(trash_stage);
    reject(candidate);
}

#[test]
#[cfg(unix)]
fn decoded_endpoint_shape_cannot_be_relabeled_as_another_kind() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let manifest = blake3::hash(b"manifest").to_hex().to_string();
    let trash_id = Uuid::new_v4();
    let reject = |candidate: RenameMoveIntent| {
        write_private(
            &app.join("operation-journal")
                .join(format!("{}.json", candidate.operation_id)),
            &serde_json::to_vec(&candidate).unwrap(),
        );
        assert!(journal.read(candidate.operation_id).is_err());
    };

    // Every valid endpoint shape is rejected when relabeled as any other kind.
    for source_shape in 0..4 {
        for target_kind in 0..4 {
            if source_shape == target_kind {
                continue;
            }
            let operation_id = Uuid::new_v4();
            let mut candidate = match source_shape {
                0 => RenameMoveIntent::new(
                    operation_id,
                    "source.md",
                    "destination.md",
                    revision("note"),
                )
                .unwrap(),
                1 => RenameMoveIntent::new_case_rename(
                    operation_id,
                    "Note.md",
                    "note.md",
                    revision("note"),
                    format!(".rename-stage/{operation_id}.tmp"),
                )
                .unwrap(),
                2 => RenameMoveIntent::new_trash(
                    operation_id,
                    trash_id,
                    manifest.clone(),
                    "source.md",
                    revision("note"),
                )
                .unwrap(),
                3 => RenameMoveIntent::new_restore(
                    operation_id,
                    trash_id,
                    manifest.clone(),
                    "restored.md",
                    revision("note"),
                )
                .unwrap(),
                _ => unreachable!(),
            };
            candidate.kind = match target_kind {
                0 => RecoveryOperationKind::NormalMove,
                1 => RecoveryOperationKind::CaseRename,
                2 => RecoveryOperationKind::Trash {
                    trash_id,
                    manifest_blake3: manifest.clone(),
                },
                3 => RecoveryOperationKind::Restore {
                    trash_id,
                    manifest_blake3: manifest.clone(),
                },
                _ => unreachable!(),
            };
            reject(candidate);
        }
    }
}

#[test]
#[cfg(unix)]
fn legacy_and_noncanonical_journal_bytes_are_never_reinterpreted() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();

    let legacy_id = Uuid::new_v4();
    let legacy = serde_json::json!({
        "version": 2,
        "operation_id": legacy_id,
        "from": "source.md",
        "to": "destination.md",
        "expected": revision("note"),
        "temp": null,
        "case_rename": false
    });
    write_private(
        &app.join("operation-journal")
            .join(format!("{legacy_id}.json")),
        &serde_json::to_vec(&legacy).unwrap(),
    );
    assert!(matches!(
        journal.read(legacy_id),
        Err(Error::UnsupportedVersion(2))
    ));

    let expected = intent();
    let canonical = serde_json::to_string(&expected).unwrap();
    let noncanonical = canonical.replacen(
        &expected.operation_id.to_string(),
        &expected.operation_id.to_string().to_uppercase(),
        1,
    );
    write_private(
        &app.join("operation-journal")
            .join(format!("{}.json", expected.operation_id)),
        noncanonical.as_bytes(),
    );
    assert!(matches!(
        journal.read(expected.operation_id),
        Err(Error::InvalidEntryName)
    ));
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rejects_extended_acls_while_modes_remain_private() {
    let (_temporary, app, vault) = roots();
    add_extended_acl(&app, 0o700);
    assert!(matches!(
        RecoveryJournal::open(&app, &vault),
        Err(Error::ExtendedAcl)
    ));

    let (_temporary, app, vault) = roots();
    let _journal = RecoveryJournal::open(&app, &vault).unwrap();
    let journal_dir = app.join("operation-journal");
    add_extended_acl(&journal_dir, 0o700);
    assert!(matches!(
        RecoveryJournal::open(&app, &vault),
        Err(Error::ExtendedAcl)
    ));

    let expected = intent();
    // Use a clean root because the journal-directory ACL above deliberately
    // prevents any further trusted journal operation.
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    journal.publish(&expected).unwrap();
    let final_path = app
        .join("operation-journal")
        .join(format!("{}.json", expected.operation_id));
    add_extended_acl(&final_path, 0o600);
    assert!(matches!(
        journal.read(expected.operation_id),
        Err(Error::ExtendedAcl)
    ));
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
