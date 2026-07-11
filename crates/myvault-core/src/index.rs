use std::path::{Path, PathBuf};

use cap_fs_ext::{FollowSymlinks, OpenOptionsExt, OpenOptionsFollowExt};
use cap_std::fs::{OpenOptions, Permissions};
use rusqlite::{params, Connection, Transaction};

use crate::capability::open_absolute_dir_nofollow;
use crate::{CoreError, Result, Vault, VaultPath};

pub const SCHEMA_VERSION: i64 = 1;
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
        if canonical_root == vault.root() || canonical_root.starts_with(vault.root()) {
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
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .follow(FollowSymlinks::No);
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
        transaction.execute(
            "DELETE FROM notes WHERE path = ?1",
            [path.as_path().to_string_lossy().as_ref()],
        )?;
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
        let mut rows = statement.query([path.as_path().to_string_lossy().as_ref()])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let stored_path: String = row.get(0)?;
        let byte_len: i64 = row.get(4)?;
        let byte_len = u64::try_from(byte_len)
            .map_err(|_| CoreError::InvalidRecord("negative byte length"))?;
        Ok(Some(NoteRecord {
            path: VaultPath::new(stored_path)?,
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
    if current < 1 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE notes (
                path TEXT PRIMARY KEY NOT NULL,
                title TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                modified_ms INTEGER NOT NULL,
                byte_len INTEGER NOT NULL CHECK (byte_len >= 0)
             );
             CREATE INDEX notes_title_idx ON notes(title COLLATE NOCASE);
             PRAGMA user_version = 1;",
        )?;
        transaction.commit()?;
    }
    Ok(())
}

fn insert_record(transaction: &Transaction<'_>, record: &NoteRecord) -> Result<()> {
    if record.content_hash.is_empty() {
        return Err(CoreError::InvalidRecord("content hash must not be empty"));
    }
    let byte_len = i64::try_from(record.byte_len)
        .map_err(|_| CoreError::InvalidRecord("byte length exceeds SQLite INTEGER"))?;
    transaction.execute(
        "INSERT INTO notes(path, title, content_hash, modified_ms, byte_len)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET
            title = excluded.title,
            content_hash = excluded.content_hash,
            modified_ms = excluded.modified_ms,
            byte_len = excluded.byte_len",
        params![
            record.path.as_path().to_string_lossy().as_ref(),
            record.title,
            record.content_hash,
            record.modified_ms,
            byte_len
        ],
    )?;
    Ok(())
}
