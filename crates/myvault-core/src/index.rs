use std::path::{Path, PathBuf};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsExt;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::OpenOptions;
#[cfg(unix)]
use cap_std::fs::Permissions;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Transaction};

use crate::capability::open_absolute_dir_nofollow;
use crate::path::VaultPathClass;
use crate::{CoreError, Result, Vault, VaultPath};

pub const SCHEMA_VERSION: i64 = 2;
const DATABASE_NAME: &str = "myvault-index.sqlite3";

/// Rebuildable metadata extracted from a source Markdown file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoteRecord {
    pub path: VaultPath,
    pub title: String,
    pub content_hash: String,
    pub modified_ms: i64,
    pub byte_len: u64,
}

/// SQLite-backed derived index. The vault files remain the source of truth.
pub struct DerivedIndex {
    connection: Connection,
    database_path: PathBuf,
}

impl DerivedIndex {
    /// Opens the derived index in a dedicated private app-data directory.
    ///
    /// `app_data_root` must already exist, must have no symlink components,
    /// and must be outside the synced vault. The database filename is fixed so
    /// callers cannot accidentally place arbitrary database files in the vault.
    ///
    /// # Errors
    ///
    /// Returns an error when the app-data location is unsafe or the database
    /// cannot be initialized or migrated.
    pub fn open(app_data_root: impl AsRef<Path>, vault: &Vault) -> Result<Self> {
        let supplied_root = app_data_root.as_ref();
        let app_dir = open_absolute_dir_nofollow(supplied_root)?;
        let canonical_root = std::fs::canonicalize(supplied_root)?;
        if canonical_root == vault.root()
            || canonical_root.starts_with(vault.root())
            || vault.root().starts_with(&canonical_root)
        {
            return Err(CoreError::AppDataInsideVault {
                app_data: canonical_root,
                vault: vault.root().to_path_buf(),
            });
        }

        secure_app_directory(&app_dir)?;
        let database_path = canonical_root.join(DATABASE_NAME);
        if app_dir
            .symlink_metadata(DATABASE_NAME)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(CoreError::UnsafeDatabasePath(database_path));
        }

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        options.follow(FollowSymlinks::No);
        let database_file = app_dir.open_with(DATABASE_NAME, &options)?;
        if !database_file.metadata()?.is_file() {
            return Err(CoreError::UnsafeDatabasePath(database_path));
        }
        secure_database_file(&database_file)?;
        database_file.sync_all()?;
        drop(database_file);

        // rusqlite's bundled SQLite VFS accepts ambient paths only. The
        // private 0700 parent and pre-created 0600 regular file close accidental
        // symlink attacks; see `SQLITE_OPEN_RESIDUAL_RISK` for the same-user
        // adversarial rename limitation between this check and sqlite3_open_v2.
        let mut connection = Connection::open(&database_path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "DELETE")?;
        connection.pragma_update(None, "temp_store", "MEMORY")?;
        migrate(&mut connection)?;
        secure_database_path(&database_path)?;
        Ok(Self {
            connection,
            database_path,
        })
    }

    /// Inserts or replaces one derived note record transactionally.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid record values or database failures.
    pub fn upsert(&mut self, record: &NoteRecord) -> Result<()> {
        let transaction = self.connection.transaction()?;
        insert_record(&transaction, record)?;
        transaction.commit()?;
        Ok(())
    }

    /// Removes one derived note record transactionally.
    ///
    /// # Errors
    ///
    /// Returns an error when the database operation cannot be completed.
    pub fn remove(&mut self, path: &VaultPath) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM notes WHERE path = ?1", [path.as_str()])?;
        transaction.commit()?;
        Ok(())
    }

    /// Replaces all derived rows in one transaction.
    ///
    /// Any invalid record or `SQLite` error rolls the deletion back.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid records or database failures. The previous
    /// derived rows remain intact after an error.
    pub fn rebuild<'a>(&mut self, records: impl IntoIterator<Item = &'a NoteRecord>) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM notes", [])?;
        for record in records {
            insert_record(&transaction, record)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Reads one record by its vault-relative path.
    ///
    /// # Errors
    ///
    /// Returns an error for database failures or malformed stored values.
    pub fn get(&self, path: &VaultPath) -> Result<Option<NoteRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT path, title, content_hash, modified_ms, byte_len
             FROM notes WHERE path = ?1",
        )?;
        let mut rows = statement.query([path.as_str()])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let stored_path: String = row.get(0)?;
        let byte_len: i64 = row.get(4)?;
        let byte_len = u64::try_from(byte_len)
            .map_err(|_| CoreError::InvalidRecord("negative byte length"))?;
        Ok(Some(NoteRecord {
            path: VaultPath::from_portable(stored_path)?,
            title: row.get(1)?,
            content_hash: row.get(2)?,
            modified_ms: row.get(3)?,
            byte_len,
        }))
    }

    /// Returns the number of derived note records.
    ///
    /// # Errors
    ///
    /// Returns an error when the count cannot be queried or represented.
    pub fn count(&self) -> Result<u64> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))?;
        u64::try_from(count).map_err(|_| CoreError::InvalidRecord("negative row count"))
    }

    /// Returns the applied database schema version.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema version cannot be queried.
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }
}

/// Exact residual blocker in the portable `SQLite` boundary.
///
/// `rusqlite`/bundled `SQLite` has no descriptor-relative `openat` VFS. A hostile
/// process running as the same OS user could rename the private app-data
/// directory in the narrow interval before `sqlite3_open_v2`. Eliminating this
/// requires a maintained custom `SQLite` VFS per platform. myVault instead holds
/// a no-follow directory capability, enforces a private 0700 directory,
/// pre-creates a no-follow 0600 regular file, and rechecks permissions after
/// open. This protects against synced-vault placement and accidental symlinks,
/// but is not a security boundary against another process with the same UID.
pub const SQLITE_OPEN_RESIDUAL_RISK: &str = "bundled SQLite opens ambient paths; a custom descriptor-relative VFS is required to resist a hostile same-user directory rename during sqlite3_open_v2";

#[cfg(unix)]
fn secure_app_directory(directory: &cap_std::fs::Dir) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    directory.set_permissions(
        ".",
        Permissions::from_std(std::fs::Permissions::from_mode(0o700)),
    )?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_app_directory(_directory: &cap_std::fs::Dir) -> Result<()> {
    // Windows ACL inheritance belongs to the platform app-data adapter. The
    // no-follow handle is opened without delete sharing by cap-std.
    Ok(())
}

#[cfg(unix)]
fn secure_database_file(file: &cap_std::fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(Permissions::from_std(std::fs::Permissions::from_mode(
        0o600,
    )))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_database_file(_file: &cap_std::fs::File) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_database_path(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(CoreError::UnsafeDatabasePath(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_database_path(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(CoreError::UnsafeDatabasePath(path.to_path_buf()));
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(CoreError::InvalidRecord(
            "database schema is newer than this application",
        ));
    }
    if current < SCHEMA_VERSION || !schema_v2_is_valid(connection)? {
        let transaction = connection.transaction()?;
        // This database contains rebuildable derived data only. Recreate it
        // instead of preserving legacy lossy paths or trusting a partial schema
        // left behind by an interrupted/older initialization.
        transaction.execute_batch(
            "DROP INDEX IF EXISTS notes_title_idx;
             DROP TABLE IF EXISTS notes;",
        )?;
        create_schema_v2(&transaction)?;
        transaction.commit()?;
    }
    Ok(())
}

fn schema_v2_is_valid(connection: &Connection) -> Result<bool> {
    let mut columns = connection.prepare("PRAGMA table_info(notes)")?;
    let columns = columns
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected_columns = [
        ("path", "TEXT", 1, 1),
        ("collision_key", "TEXT", 1, 0),
        ("title", "TEXT", 1, 0),
        ("content_hash", "TEXT", 1, 0),
        ("modified_ms", "INTEGER", 1, 0),
        ("byte_len", "INTEGER", 1, 0),
    ];
    if columns.len() != expected_columns.len()
        || columns.iter().zip(expected_columns).any(
            |((name, data_type, not_null, primary_key), expected)| {
                (name.as_str(), data_type.as_str(), *not_null, *primary_key) != expected
            },
        )
    {
        return Ok(false);
    }

    let mut indexes = connection.prepare("PRAGMA index_list(notes)")?;
    let indexes = indexes
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut has_collision_unique = false;
    let mut has_title_index = false;
    for (name, unique) in indexes {
        let columns = index_key_columns(connection, &name)?;
        has_collision_unique |= unique
            && matches!(
                columns.as_slice(),
                [(column, collation)] if column == "collision_key" && collation == "BINARY"
            );
        has_title_index |= name == "notes_title_idx"
            && matches!(
                columns.as_slice(),
                [(column, collation)] if column == "title" && collation == "NOCASE"
            );
    }
    Ok(has_collision_unique && has_title_index)
}

fn index_key_columns(connection: &Connection, index_name: &str) -> Result<Vec<(String, String)>> {
    let escaped_name = index_name.replace('"', "\"\"");
    let mut statement = connection.prepare(&format!("PRAGMA index_xinfo(\"{escaped_name}\")"))?;
    let columns = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(5)? != 0,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .filter_map(|row| match row {
            Ok((true, Some(name), collation)) => Some(Ok((name, collation))),
            Ok((_, _name, _collation)) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(columns)
}

fn create_schema_v2(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE notes (
            path TEXT PRIMARY KEY NOT NULL,
            collision_key TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            modified_ms INTEGER NOT NULL,
            byte_len INTEGER NOT NULL CHECK (byte_len >= 0)
         );
         CREATE INDEX notes_title_idx ON notes(title COLLATE NOCASE);
         PRAGMA user_version = 2;",
    )?;
    Ok(())
}

fn insert_record(transaction: &Transaction<'_>, record: &NoteRecord) -> Result<()> {
    if record.path.classify() != VaultPathClass::Content {
        return Err(CoreError::InvalidRecord(
            "internal .obsidian and .trash paths must not be indexed",
        ));
    }
    if record.content_hash.is_empty() {
        return Err(CoreError::InvalidRecord("content hash must not be empty"));
    }
    let byte_len = i64::try_from(record.byte_len)
        .map_err(|_| CoreError::InvalidRecord("byte length exceeds SQLite INTEGER"))?;
    let collision_key = record.path.collision_key();
    if let Some(existing) = find_collision(transaction, &collision_key, &record.path)? {
        return Err(CoreError::PortablePathCollision {
            existing,
            incoming: record.path.as_str().to_owned(),
        });
    }
    let inserted = transaction.execute(
        "INSERT INTO notes(path, collision_key, title, content_hash, modified_ms, byte_len)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET
            collision_key = excluded.collision_key,
            title = excluded.title,
            content_hash = excluded.content_hash,
            modified_ms = excluded.modified_ms,
            byte_len = excluded.byte_len",
        params![
            record.path.as_str(),
            collision_key,
            record.title,
            record.content_hash,
            record.modified_ms,
            byte_len
        ],
    );
    if let Err(error) = inserted {
        // SQLite serializes writers in the configured DELETE journal mode, so
        // this is mainly a defense for a future journal/VFS change. If another
        // connection wins after the preflight query, translate the UNIQUE
        // constraint into the same deterministic domain error.
        if matches!(
            &error,
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.code == ErrorCode::ConstraintViolation
        ) {
            if let Some(existing) = find_collision(transaction, &collision_key, &record.path)? {
                return Err(CoreError::PortablePathCollision {
                    existing,
                    incoming: record.path.as_str().to_owned(),
                });
            }
        }
        return Err(error.into());
    }
    Ok(())
}

fn find_collision(
    transaction: &Transaction<'_>,
    collision_key: &str,
    incoming: &VaultPath,
) -> Result<Option<String>> {
    Ok(transaction
        .query_row(
            "SELECT path FROM notes WHERE collision_key = ?1 AND path <> ?2 LIMIT 1",
            params![collision_key, incoming.as_str()],
            |row| row.get(0),
        )
        .optional()?)
}
