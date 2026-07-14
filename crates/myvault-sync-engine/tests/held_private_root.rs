use myvault_private_fs::{self as private_fs, HeldPrivateRoot};
use myvault_sync_engine::{
    BindOutcome, Error, SyncStore, TransferDirection, TransferMimeClass, TransferPhase,
    TransferRecord, VerifiedRemoteBinding,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    _temporary: TempDir,
    app_data: PathBuf,
    root: HeldPrivateRoot,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary root");
        let base = temporary.path().canonicalize().expect("canonical root");
        let app_data = base.join("no-backup");
        let vault = base.join("vault");
        fs::create_dir(&app_data).expect("app data");
        fs::create_dir(&vault).expect("vault");
        make_private(&app_data);
        let root = private_fs::open_private_disjoint_held_root(&app_data, &vault)
            .expect("held private root");
        Self {
            _temporary: temporary,
            app_data,
            root,
        }
    }

    fn open(&self, vault_id: Uuid) -> SyncStore {
        SyncStore::open_from_held_private_root(&self.root, vault_id).expect("sync store")
    }
}

#[cfg(unix)]
fn make_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private mode");
}

#[cfg(not(unix))]
fn make_private(_path: &Path) {}

fn binding(account: &str, root: &str) -> VerifiedRemoteBinding {
    VerifiedRemoteBinding::new(account, root, account, root).expect("binding")
}

fn hash(byte: u8) -> String {
    std::iter::repeat_n(char::from(byte), 64).collect()
}

fn upload(operation_id: Uuid) -> TransferRecord {
    TransferRecord::new(
        operation_id,
        TransferDirection::Upload,
        "notes/held.md",
        "remote-parent",
        None,
        Some(hash(b'a')),
        None,
        hash(b'b'),
        42,
        TransferMimeClass::Markdown,
        format!("held-{operation_id}"),
        Some(format!("stage.{operation_id}")),
        None,
        10,
    )
    .expect("upload")
}

#[test]
fn held_root_keeps_vault_databases_and_bindings_isolated() {
    let fixture = Fixture::new();
    let vault_a = Uuid::new_v4();
    let vault_b = Uuid::new_v4();
    let path_a;
    {
        let mut store = fixture.open(vault_a);
        assert_eq!(
            store
                .bind_remote_root(&binding("account-a", "root-a"), 1)
                .unwrap(),
            BindOutcome::Created
        );
        path_a = store.database_path().to_owned();
    }

    let mut store_b = fixture.open(vault_b);
    assert!(store_b.vault_state().unwrap().is_none());
    assert_eq!(
        store_b
            .bind_remote_root(&binding("account-b", "root-b"), 2)
            .unwrap(),
        BindOutcome::Created
    );
    let path_b = store_b.database_path().to_owned();
    assert_ne!(path_a, path_b);
    assert!(path_a.ends_with(
        Path::new("sync-state")
            .join("v1")
            .join("vaults")
            .join(vault_a.to_string())
            .join("myvault-sync.sqlite3")
    ));
    assert!(path_b.ends_with(
        Path::new("sync-state")
            .join("v1")
            .join("vaults")
            .join(vault_b.to_string())
            .join("myvault-sync.sqlite3")
    ));
    drop(store_b);

    let reopened_a = fixture.open(vault_a);
    assert_eq!(
        reopened_a
            .vault_state()
            .unwrap()
            .unwrap()
            .account_id
            .as_deref(),
        Some("account-a")
    );
}

#[test]
fn held_root_enforces_same_vault_lease_but_allows_other_vault() {
    let fixture = Fixture::new();
    let vault_a = Uuid::new_v4();
    let vault_b = Uuid::new_v4();
    let first = fixture.open(vault_a);
    assert!(matches!(
        SyncStore::open_from_held_private_root(&fixture.root, vault_a),
        Err(Error::SyncLeaseHeld)
    ));
    let other = fixture.open(vault_b);
    drop(other);
    drop(first);
    fixture.open(vault_a);
}

#[test]
fn held_root_restart_recovers_running_transfer_to_reconcile() {
    let fixture = Fixture::new();
    let vault_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    {
        let mut store = fixture.open(vault_id);
        store
            .bind_remote_root(&binding("account-a", "root-a"), 1)
            .unwrap();
        store.register_transfer(&upload(operation_id)).unwrap();
        assert_eq!(
            store.claim_next_transfer(10).unwrap().unwrap().phase,
            TransferPhase::Running
        );
    }

    let reopened = fixture.open(vault_id);
    let transfer = reopened.transfer(operation_id).unwrap().unwrap();
    assert_eq!(transfer.phase, TransferPhase::NeedsReconcile);
    assert_eq!(
        transfer.last_error_code.as_deref(),
        Some("interrupted_unknown_outcome")
    );
}

#[cfg(unix)]
#[test]
fn held_root_rejects_permission_drift_and_named_replacement() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fs::set_permissions(&fixture.app_data, fs::Permissions::from_mode(0o755))
        .expect("weaken permissions");
    assert!(SyncStore::open_from_held_private_root(&fixture.root, Uuid::new_v4()).is_err());
    fs::set_permissions(&fixture.app_data, fs::Permissions::from_mode(0o700))
        .expect("restore permissions");

    let moved = fixture.app_data.with_extension("moved");
    fs::rename(&fixture.app_data, &moved).expect("move original root");
    fs::create_dir(&fixture.app_data).expect("replacement root");
    fs::set_permissions(&fixture.app_data, fs::Permissions::from_mode(0o700))
        .expect("replacement permissions");
    assert!(SyncStore::open_from_held_private_root(&fixture.root, Uuid::new_v4()).is_err());
}

#[cfg(unix)]
#[test]
fn held_root_rejects_symlinked_vault_directory_and_malformed_database() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let fixture = Fixture::new();
    let root = fixture.root.try_clone_directory().unwrap();
    let sync = private_fs::create_or_open_private_dir(&root, "sync-state").unwrap();
    let version = private_fs::create_or_open_private_dir(&sync, "v1").unwrap();
    let vaults = private_fs::create_or_open_private_dir(&version, "vaults").unwrap();
    let symlinked_id = Uuid::new_v4();
    let target = fixture.app_data.join("symlink-target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(
        &target,
        fixture
            .app_data
            .join("sync-state/v1/vaults")
            .join(symlinked_id.to_string()),
    )
    .unwrap();
    assert!(SyncStore::open_from_held_private_root(&fixture.root, symlinked_id).is_err());

    let malformed_id = Uuid::new_v4();
    let malformed = private_fs::create_or_open_private_dir(&vaults, malformed_id.to_string())
        .expect("malformed vault dir");
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = malformed
        .open_with("myvault-sync.sqlite3", &options)
        .expect("malformed database");
    private_fs::set_private_file_permissions(&file).unwrap();
    file.write_all(b"not sqlite").unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert!(SyncStore::open_from_held_private_root(&fixture.root, malformed_id).is_err());
}
