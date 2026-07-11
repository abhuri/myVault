use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use myvault_core::{
    BurstNormalizer, CoreError, DerivedIndex, NormalizedEvent, NoteRecord, RawEvent,
    SelfWriteSuppressor, Vault, VaultPath, WriteFingerprint, WriteIntent, SCHEMA_VERSION,
};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let path = temp_root.join(format!("myvault-core-{label}-{}-{id}", std::process::id()));
        fs::create_dir(&path).expect("create isolated test directory");
        Self(path)
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove isolated test directory");
    }
}

fn note(path: &str, title: &str, hash: &str) -> NoteRecord {
    NoteRecord {
        path: VaultPath::new(path).expect("valid fixture path"),
        title: title.to_owned(),
        content_hash: hash.to_owned(),
        modified_ms: 1_700_000_000_000,
        byte_len: 42,
    }
}

#[test]
fn atomic_write_supports_thai_unicode_spaces_and_replacement() {
    let root = TestDir::new("atomic");
    fs::create_dir(root.as_ref().join("บันทึก ประจำวัน")).expect("create note folder");
    let vault = Vault::open(&root).expect("open vault");
    let path = VaultPath::new("บันทึก ประจำวัน/你好 world.md").expect("valid path");

    vault
        .atomic_write(&path, "รุ่นแรก 👋".as_bytes(), WriteIntent::Automatic)
        .expect("initial atomic write");
    vault
        .atomic_write(&path, "รุ่นที่สอง ✅".as_bytes(), WriteIntent::Automatic)
        .expect("atomic replacement");

    assert_eq!(
        fs::read_to_string(root.as_ref().join(path.as_path())).expect("read note"),
        "รุ่นที่สอง ✅"
    );
}

#[test]
fn automatic_obsidian_writes_are_denied_but_explicit_user_writes_are_allowed() {
    let root = TestDir::new("obsidian");
    fs::create_dir(root.as_ref().join(".obsidian")).expect("create metadata folder");
    let vault = Vault::open(&root).expect("open vault");
    let path = VaultPath::new(".obsidian/app.json").expect("valid path");

    let error = vault
        .atomic_write(&path, b"{}", WriteIntent::Automatic)
        .expect_err("automatic metadata write must fail");
    assert!(matches!(error, CoreError::AutomaticObsidianWriteDenied(_)));
    assert!(!root.as_ref().join(path.as_path()).exists());

    vault
        .atomic_write(&path, b"{}", WriteIntent::UserInitiated)
        .expect("explicit metadata write");
}

#[cfg(unix)]
#[test]
fn symlink_escape_is_rejected_for_reads_and_writes() {
    use std::os::unix::fs::symlink;

    let root = TestDir::new("vault");
    let outside = TestDir::new("outside");
    fs::write(outside.as_ref().join("secret.md"), "outside").expect("write outside fixture");
    symlink(outside.as_ref(), root.as_ref().join("escape")).expect("create escape symlink");
    let vault = Vault::open(&root).expect("open vault");

    for path in ["escape/secret.md", "escape/new.md"] {
        let path = VaultPath::new(path).expect("valid relative fixture");
        assert!(matches!(
            vault.read(&path),
            Err(CoreError::SymlinkRejected(_))
        ));
        assert!(matches!(
            vault.atomic_write(&path, b"blocked", WriteIntent::Automatic),
            Err(CoreError::SymlinkRejected(_))
        ));
    }
    assert!(!outside.as_ref().join("new.md").exists());
}

#[test]
fn watcher_bursts_are_normalized_and_self_writes_require_exact_fingerprints() {
    let changed = VaultPath::new("โน้ต งาน.md").expect("path");
    let deleted = VaultPath::new("เก่า.md").expect("path");
    let renamed = VaultPath::new("ใหม่.md").expect("path");
    let mut burst = BurstNormalizer::default();
    burst.push(RawEvent::Create(changed.clone()));
    burst.push(RawEvent::Modify(changed.clone()));
    burst.push(RawEvent::Delete(deleted.clone()));
    burst.push(RawEvent::Rename {
        from: deleted.clone(),
        to: renamed.clone(),
    });

    assert_eq!(
        burst.finish(),
        vec![
            NormalizedEvent::Rename {
                from: deleted,
                to: renamed,
            },
            NormalizedEvent::Upsert(changed.clone()),
        ]
    );

    let ours = WriteFingerprint {
        byte_len: 12,
        content_tag: 99,
    };
    let external = WriteFingerprint {
        byte_len: 12,
        content_tag: 100,
    };
    let mut suppressor = SelfWriteSuppressor::default();
    suppressor.record(changed.clone(), ours, 20);
    assert!(!suppressor.should_suppress(&changed, external, 11));
    assert!(suppressor.should_suppress(&changed, ours, 12));
    assert!(!suppressor.should_suppress(&changed, ours, 13));
    suppressor.record(changed.clone(), ours, 20);
    assert!(!suppressor.should_suppress(&changed, ours, 21));
}

#[test]
fn sqlite_migration_is_idempotent_and_unicode_records_round_trip() {
    let vault_root = TestDir::new("sqlite-vault");
    let app_data = TestDir::new("sqlite-app-data");
    let vault = Vault::open(&vault_root).expect("open vault");
    let expected = note("โครงการ/สวัสดี 世界.md", "หัวข้อ 世界", "abc123");

    {
        let mut index = DerivedIndex::open(&app_data, &vault).expect("create index");
        assert_eq!(index.schema_version().expect("version"), SCHEMA_VERSION);
        index.upsert(&expected).expect("insert record");
    }
    let index = DerivedIndex::open(&app_data, &vault).expect("reopen and rerun migration");
    assert_eq!(
        index.get(&expected.path).expect("read record"),
        Some(expected)
    );
}

#[test]
fn sqlite_v1_migration_discards_legacy_paths_and_reopens_as_schema_v2() {
    let vault_root = TestDir::new("sqlite-v1-vault");
    let app_data = TestDir::new("sqlite-v1-app-data");
    let vault = Vault::open(&vault_root).expect("open vault");
    let database_path = app_data.as_ref().join("myvault-index.sqlite3");

    {
        let connection = rusqlite::Connection::open(&database_path).expect("create v1 database");
        connection
            .execute_batch(
                "CREATE TABLE notes (
                    path TEXT PRIMARY KEY NOT NULL,
                    title TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    modified_ms INTEGER NOT NULL,
                    byte_len INTEGER NOT NULL CHECK (byte_len >= 0)
                 );
                 CREATE INDEX notes_title_idx ON notes(title COLLATE NOCASE);
                 INSERT INTO notes(path, title, content_hash, modified_ms, byte_len)
                 VALUES ('legacy\\windows\\note.md', 'legacy', 'old', 1, 2);
                 PRAGMA user_version = 1;",
            )
            .expect("seed v1 database");
    }

    {
        let index = DerivedIndex::open(&app_data, &vault).expect("migrate v1 database");
        assert_eq!(index.schema_version().expect("schema version"), 2);
        assert_eq!(index.count().expect("v1 rows were discarded"), 0);
    }

    let index = DerivedIndex::open(&app_data, &vault).expect("idempotent schema v2 reopen");
    assert_eq!(index.schema_version().expect("schema version"), 2);
    assert_eq!(index.count().expect("still empty"), 0);
}

#[test]
fn portable_path_collisions_are_rejected_and_rebuild_rolls_back() {
    let vault_root = TestDir::new("collision-vault");
    let app_data = TestDir::new("collision-app-data");
    let vault = Vault::open(&vault_root).expect("open vault");
    let mut index = DerivedIndex::open(&app_data, &vault).expect("open index");
    let existing = note("Notes/Ｃａｆé.md", "existing", "first");
    let collision = note("notes/Cafe\u{301}.md", "collision", "second");
    assert_eq!(
        existing.path.collision_key(),
        collision.path.collision_key()
    );

    index.upsert(&existing).expect("seed collision key");
    assert!(matches!(
        index.upsert(&collision),
        Err(CoreError::PortablePathCollision {
            existing: stored,
            incoming
        }) if stored == existing.path.as_str() && incoming == collision.path.as_str()
    ));
    assert_eq!(index.count().expect("failed upsert count"), 1);
    assert_eq!(
        index.get(&existing.path).expect("existing after upsert"),
        Some(existing.clone())
    );

    let unrelated = note("unrelated.md", "unrelated", "third");
    assert!(matches!(
        index.rebuild([&unrelated, &existing, &collision]),
        Err(CoreError::PortablePathCollision { .. })
    ));
    assert_eq!(index.count().expect("failed rebuild count"), 1);
    assert_eq!(
        index.get(&existing.path).expect("existing after rebuild"),
        Some(existing)
    );
    assert_eq!(
        index.get(&unrelated.path).expect("unrelated rolled back"),
        None
    );
}

#[test]
fn failed_rebuild_rolls_back_and_successful_rebuild_replaces_derived_rows() {
    let vault_root = TestDir::new("rebuild-vault");
    let app_data = TestDir::new("rebuild-app-data");
    let vault = Vault::open(&vault_root).expect("open vault");
    let mut index = DerivedIndex::open(&app_data, &vault).expect("open index");
    let original = note("เดิม.md", "เดิม", "old");
    index.upsert(&original).expect("seed index");

    let invalid = note("เสีย.md", "เสีย", "");
    assert!(matches!(
        index.rebuild([&invalid]),
        Err(CoreError::InvalidRecord(_))
    ));
    assert_eq!(index.count().expect("count after rollback"), 1);
    assert_eq!(
        index.get(&original.path).expect("original after rollback"),
        Some(original)
    );

    let first = note("ใหม่ หนึ่ง.md", "หนึ่ง", "new-1");
    let second = note("ใหม่/สอง.md", "สอง", "new-2");
    index
        .rebuild([&first, &second])
        .expect("rebuild derived index");
    assert_eq!(index.count().expect("rebuilt count"), 2);
    assert_eq!(index.get(&first.path).expect("first"), Some(first));
    assert_eq!(index.get(&second.path).expect("second"), Some(second));
}

#[test]
fn index_excludes_obsidian_metadata_and_trash_from_upsert_and_rebuild() {
    let vault_root = TestDir::new("internal-index-vault");
    let app_data = TestDir::new("internal-index-app-data");
    let vault = Vault::open(&vault_root).expect("open vault");
    let mut index = DerivedIndex::open(&app_data, &vault).expect("open index");
    let content = note("เก็บ.md", "เก็บ", "content");
    index.upsert(&content).expect("seed content");

    for internal in [
        note(".obsidian/app.json", "metadata", "obsidian"),
        note(".trash/ลบแล้ว.md", "deleted", "trash"),
    ] {
        assert!(matches!(
            index.upsert(&internal),
            Err(CoreError::InvalidRecord(_))
        ));
        assert!(matches!(
            index.rebuild([&internal]),
            Err(CoreError::InvalidRecord(_))
        ));
        assert_eq!(index.count().expect("rollback count"), 1);
        assert_eq!(
            index.get(&content.path).expect("content after rollback"),
            Some(content.clone())
        );
    }
}

#[test]
fn index_rejects_app_data_inside_the_synced_vault() {
    let vault_root = TestDir::new("placement-vault");
    fs::create_dir(vault_root.as_ref().join(".myvault-data")).expect("app-data fixture");
    let vault = Vault::open(&vault_root).expect("open vault");

    assert!(matches!(
        DerivedIndex::open(vault_root.as_ref().join(".myvault-data"), &vault),
        Err(CoreError::AppDataInsideVault { .. })
    ));
}

#[cfg(unix)]
#[test]
fn index_rejects_symlink_database_and_symlink_app_data_components() {
    use std::os::unix::fs::symlink;

    let vault_root = TestDir::new("index-symlink-vault");
    let vault = Vault::open(&vault_root).expect("open vault");
    let app_data = TestDir::new("index-symlink-app");
    let outside = TestDir::new("index-symlink-outside");
    fs::write(outside.as_ref().join("database"), b"not sqlite").expect("outside file");
    symlink(
        outside.as_ref().join("database"),
        app_data.as_ref().join("myvault-index.sqlite3"),
    )
    .expect("database symlink");
    assert!(matches!(
        DerivedIndex::open(&app_data, &vault),
        Err(CoreError::UnsafeDatabasePath(_))
    ));

    let parent = TestDir::new("index-symlink-parent");
    symlink(app_data.as_ref(), parent.as_ref().join("linked-app-data")).expect("app-data symlink");
    assert!(matches!(
        DerivedIndex::open(parent.as_ref().join("linked-app-data"), &vault),
        Err(CoreError::SymlinkRejected(_))
    ));
}

#[cfg(unix)]
#[test]
fn index_enforces_private_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let vault_root = TestDir::new("permissions-vault");
    let app_data = TestDir::new("permissions-app");
    fs::set_permissions(app_data.as_ref(), fs::Permissions::from_mode(0o755))
        .expect("make fixture overly broad");
    let vault = Vault::open(&vault_root).expect("open vault");
    let index = DerivedIndex::open(&app_data, &vault).expect("open index");

    let directory_mode = fs::metadata(app_data.as_ref())
        .expect("directory metadata")
        .permissions()
        .mode()
        & 0o777;
    let database_mode = fs::metadata(index.database_path())
        .expect("database metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(directory_mode, 0o700);
    assert_eq!(database_mode, 0o600);
}
