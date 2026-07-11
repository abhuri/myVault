use myvault_recovery::{
    decide_recovery, Error, FileRevision, RecoveryDecision, RecoveryJournal, RecoveryTopology,
    RenameMoveIntent,
};
use std::fs;
use std::io::Write;
use tempfile::TempDir;
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
    (temporary, app, vault)
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
                from: Some(other),
                to: None,
                temp: Some(expected.clone()),
            },
            RecoveryDecision::ExternalMutation,
        ),
    ];
    for (topology, expected_decision) in cases {
        assert_eq!(decide_recovery(&intent, &topology), expected_decision);
    }
}

#[test]
fn round_trips_thai_paths_and_lists_entries() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let intent = intent();
    journal.publish(&intent).unwrap();
    assert_eq!(journal.read(intent.operation_id).unwrap(), intent);
    assert_eq!(journal.list().unwrap(), vec![intent]);
}

#[test]
fn malformed_json_is_rejected() {
    let (_temporary, app, vault) = roots();
    let journal = RecoveryJournal::open(&app, &vault).unwrap();
    let id = Uuid::new_v4();
    fs::write(
        app.join("operation-journal").join(format!("{id}.json")),
        b"{",
    )
    .unwrap();
    assert!(matches!(journal.read(id), Err(Error::Json(_))));
}

#[test]
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
}

#[test]
fn rejects_overlapping_roots() {
    let temporary = TempDir::new().unwrap();
    let base = temporary.path().canonicalize().unwrap();
    let app = base.join("app");
    let vault = app.join("vault");
    fs::create_dir(&app).unwrap();
    fs::create_dir(&vault).unwrap();
    assert!(matches!(
        RecoveryJournal::open(&app, &vault),
        Err(Error::InvalidRoot(_))
    ));
}
