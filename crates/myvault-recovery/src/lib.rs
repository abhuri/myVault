#![forbid(unsafe_code)]

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use myvault_core::VaultPath;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

const JOURNAL_DIRECTORY: &str = "operation-journal";
const MAX_ENTRY_BYTES: u64 = 64 * 1024;
const MAX_ENTRY_COUNT: usize = 4096;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidRoot(&'static str),
    InvalidRevision,
    InvalidPortablePath,
    InvalidEntryName,
    EntryTooLarge,
    TooManyEntries,
    UnsupportedVersion(u32),
    PublishedButNotSynced(io::Error),
    PublishedCleanupFailed(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "invalid journal JSON: {error}"),
            Self::InvalidRoot(reason) => write!(formatter, "invalid recovery root: {reason}"),
            Self::InvalidRevision => formatter.write_str("invalid BLAKE3 revision"),
            Self::InvalidPortablePath => formatter.write_str("invalid portable vault path"),
            Self::InvalidEntryName => formatter.write_str("invalid journal entry name"),
            Self::EntryTooLarge => formatter.write_str("journal entry exceeds size limit"),
            Self::TooManyEntries => formatter.write_str("journal contains too many entries"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported journal version {version}")
            }
            Self::PublishedButNotSynced(error) => {
                write!(
                    formatter,
                    "journal published but directory sync failed: {error}"
                )
            }
            Self::PublishedCleanupFailed(error) => {
                write!(
                    formatter,
                    "journal published but temp cleanup failed: {error}"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRevision {
    pub blake3_hex: String,
    pub byte_len: u64,
}

impl FileRevision {
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            blake3_hex: blake3::hash(bytes).to_hex().to_string(),
            byte_len: bytes.len() as u64,
        }
    }

    /// # Errors
    ///
    /// Returns [`Error::InvalidRevision`] unless the digest is canonical lowercase BLAKE3 hex.
    pub fn validate(&self) -> Result<(), Error> {
        let valid = self.blake3_hex.len() == 64
            && self
                .blake3_hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(())
        } else {
            Err(Error::InvalidRevision)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameMoveIntent {
    pub version: u32,
    pub operation_id: Uuid,
    pub from: String,
    pub to: String,
    pub expected: FileRevision,
    pub temp: Option<String>,
}

impl RenameMoveIntent {
    pub const VERSION: u32 = 1;

    /// # Errors
    ///
    /// Returns an error when the revision or any portable vault path is invalid.
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        expected: FileRevision,
        temp: Option<String>,
    ) -> Result<Self, Error> {
        expected.validate()?;
        let from = from.into();
        let to = to.into();
        validate_portable_path(&from)?;
        validate_portable_path(&to)?;
        if let Some(path) = &temp {
            validate_portable_path(path)?;
        }
        Ok(Self {
            version: Self::VERSION,
            operation_id: Uuid::new_v4(),
            from,
            to,
            expected,
            temp,
        })
    }

    fn validate(&self) -> Result<(), Error> {
        if self.version != Self::VERSION {
            return Err(Error::UnsupportedVersion(self.version));
        }
        self.expected.validate()?;
        validate_portable_path(&self.from)?;
        validate_portable_path(&self.to)?;
        if let Some(path) = &self.temp {
            validate_portable_path(path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryDecision {
    NotStarted,
    InProgressAtTemp,
    Committed,
    DestinationCollision,
    DuplicateManual,
    DataLoss,
    ExternalMutation,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryTopology {
    pub from: Option<FileRevision>,
    pub to: Option<FileRevision>,
    pub temp: Option<FileRevision>,
}

/// Classifies observed topology conservatively. It never mutates or deletes data.
#[must_use]
pub fn decide_recovery(intent: &RenameMoveIntent, topology: &RecoveryTopology) -> RecoveryDecision {
    let expected = &intent.expected;
    match (&topology.from, &topology.to, &topology.temp) {
        (Some(from), None, None) if from == expected => RecoveryDecision::NotStarted,
        (None, None, Some(temp)) if temp == expected => RecoveryDecision::InProgressAtTemp,
        (None, Some(to), None) if to == expected => RecoveryDecision::Committed,
        (Some(from), Some(to), _) if from == expected && to == expected => {
            RecoveryDecision::DuplicateManual
        }
        (Some(from), Some(to), _) if from == expected && to != expected => {
            RecoveryDecision::DestinationCollision
        }
        (None, None, None) => RecoveryDecision::DataLoss,
        _ => RecoveryDecision::ExternalMutation,
    }
}

pub struct RecoveryJournal {
    directory: Dir,
}

impl RecoveryJournal {
    /// Opens a dedicated journal below an existing private app-data root.
    ///
    /// # Errors
    ///
    /// Returns an error for missing, symlinked, overlapping, or inaccessible roots.
    pub fn open(app_data_root: &Path, vault_root: &Path) -> Result<Self, Error> {
        validate_disjoint(app_data_root, vault_root)?;
        let app_directory = open_absolute_dir_nofollow(app_data_root)?;
        require_private_directory(&app_directory)?;
        match app_directory.create_dir(JOURNAL_DIRECTORY) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let directory = open_child_dir_nofollow(&app_directory, JOURNAL_DIRECTORY)?;
        set_held_directory_permissions(&directory)?;
        sync_held_directory(&app_directory)?;
        Ok(Self { directory })
    }

    /// Durably publishes an intent using temp-write, file sync, rename, and directory sync.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid intents, oversized serialization, or failed I/O.
    pub fn publish(&self, intent: &RenameMoveIntent) -> Result<(), Error> {
        intent.validate()?;
        let final_name = entry_name(intent.operation_id);
        let temporary_name = format!(".{final_name}.tmp");
        let bytes = serde_json::to_vec(intent)?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Err(Error::EntryTooLarge);
        }

        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = self.directory.open_with(&temporary_name, &options)?;
        set_held_file_permissions(&file)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        self.directory
            .hard_link(&temporary_name, &self.directory, &final_name)?;
        sync_held_directory(&self.directory).map_err(published_sync_error)?;
        self.directory
            .remove_file(&temporary_name)
            .map_err(Error::PublishedCleanupFailed)?;
        sync_held_directory(&self.directory).map_err(published_sync_error)?;
        Ok(())
    }

    /// Reads and validates one bounded journal entry.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent, malformed, oversized, or mismatched entry.
    pub fn read(&self, operation_id: Uuid) -> Result<RenameMoveIntent, Error> {
        let name = entry_name(operation_id);
        let metadata = self.directory.symlink_metadata(&name)?;
        if !metadata.file_type().is_file() {
            return Err(Error::InvalidEntryName);
        }
        if metadata.len() > MAX_ENTRY_BYTES {
            return Err(Error::EntryTooLarge);
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = self.directory.open_with(&name, &options)?;
        if !file.metadata()?.is_file() {
            return Err(Error::InvalidEntryName);
        }
        set_held_file_permissions(&file)?;
        let capacity = usize::try_from(metadata.len()).map_err(|_| Error::EntryTooLarge)?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Err(Error::EntryTooLarge);
        }
        let intent: RenameMoveIntent = serde_json::from_slice(&bytes)?;
        intent.validate()?;
        if intent.operation_id != operation_id {
            return Err(Error::InvalidEntryName);
        }
        Ok(intent)
    }

    /// Lists all committed entries, ignoring crash-left temporary files.
    ///
    /// # Errors
    ///
    /// Returns an error for excessive entry counts or any invalid committed entry.
    pub fn list(&self) -> Result<Vec<RenameMoveIntent>, Error> {
        let mut ids = Vec::new();
        let mut entry_count = 0_usize;
        for entry in self.directory.entries()? {
            let entry = entry?;
            entry_count += 1;
            if entry_count > MAX_ENTRY_COUNT {
                return Err(Error::TooManyEntries);
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = parse_entry_name(name) else {
                continue;
            };
            ids.push(id);
        }
        ids.sort_unstable();
        ids.into_iter().map(|id| self.read(id)).collect()
    }
}

fn validate_portable_path(path: &str) -> Result<(), Error> {
    VaultPath::from_portable(path)
        .map(|_| ())
        .map_err(|_| Error::InvalidPortablePath)
}

fn entry_name(operation_id: Uuid) -> String {
    format!("{operation_id}.json")
}

fn parse_entry_name(name: &str) -> Option<Uuid> {
    let id = name.strip_suffix(".json")?;
    Uuid::parse_str(id).ok()
}

fn validate_disjoint(app_data_root: &Path, vault_root: &Path) -> Result<(), Error> {
    let app = app_data_root.canonicalize()?;
    let vault = vault_root.canonicalize()?;
    if app == vault || app.starts_with(&vault) || vault.starts_with(&app) {
        return Err(Error::InvalidRoot(
            "app data and vault roots must be disjoint",
        ));
    }
    Ok(())
}

fn open_absolute_dir_nofollow(path: &Path) -> Result<Dir, Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidRoot("root must be absolute"));
    }
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
            Component::RootDir => anchor.push(std::path::MAIN_SEPARATOR_STR),
            Component::Normal(name) => names.push(name.to_owned()),
            Component::CurDir | Component::ParentDir => {
                return Err(Error::InvalidRoot("root is not normalized"));
            }
        }
    }
    let mut directory = Dir::open_ambient_dir(anchor, ambient_authority())?;
    for name in names {
        directory = open_child_dir_nofollow(&directory, &name)?;
    }
    Ok(directory)
}

fn open_child_dir_nofollow(parent: &Dir, name: impl AsRef<Path>) -> Result<Dir, Error> {
    let name = name.as_ref();
    if parent
        .symlink_metadata(name)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(Error::InvalidRoot("root contains a symlink component"));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = parent.open_with(name, &options).map_err(|error| {
        if parent
            .symlink_metadata(name)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            Error::InvalidRoot("root contains a symlink component")
        } else {
            Error::Io(error)
        }
    })?;
    if !file.metadata()?.is_dir() {
        return Err(Error::InvalidRoot("root is not a directory"));
    }
    Ok(Dir::from_std_file(file.into_std()))
}

fn published_sync_error(error: Error) -> Error {
    match error {
        Error::Io(error) => Error::PublishedButNotSynced(error),
        other => other,
    }
}

#[cfg(unix)]
fn require_private_directory(directory: &Dir) -> Result<(), Error> {
    use cap_std::fs::PermissionsExt;
    if directory.dir_metadata()?.permissions().mode() & 0o077 != 0 {
        return Err(Error::InvalidRoot(
            "app data root grants group or world access",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_private_directory(_directory: &Dir) -> Result<(), Error> {
    Ok(())
}

#[cfg(unix)]
fn set_held_directory_permissions(directory: &Dir) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    directory
        .try_clone()?
        .into_std_file()
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_held_directory_permissions(_directory: &Dir) -> Result<(), Error> {
    Ok(())
}

#[cfg(unix)]
fn set_held_file_permissions(file: &cap_std::fs::File) -> Result<(), Error> {
    use cap_std::fs::{Permissions, PermissionsExt};
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_held_file_permissions(_file: &cap_std::fs::File) -> Result<(), Error> {
    Ok(())
}

fn sync_held_directory(directory: &Dir) -> Result<(), Error> {
    directory.try_clone()?.into_std_file().sync_all()?;
    Ok(())
}
