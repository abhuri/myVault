#![forbid(unsafe_code)]

#[cfg(not(windows))]
use cap_fs_ext::OpenOptionsMaybeDirExt;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
#[cfg(not(windows))]
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use myvault_core::VaultPath;
use serde::{Deserialize, Serialize};
use std::fmt;
#[cfg(unix)]
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
#[cfg(not(windows))]
use std::path::{Component, PathBuf};
use uuid::Uuid;

#[cfg(not(windows))]
const JOURNAL_DIRECTORY: &str = "operation-journal";
#[cfg(not(windows))]
const COMPLETED_DIRECTORY: &str = "completed";
const MAX_ENTRY_BYTES: u64 = 64 * 1024;
const MAX_ENTRY_COUNT: usize = 4096;
pub const MAX_PAGE_SIZE: usize = 128;

/// Recovery records are untrusted hints. They never authorize a vault mutation.
/// Callers must independently inspect the vault and apply their normal mutation policy.
pub const JOURNAL_IS_UNTRUSTED_HINT: bool = true;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidRoot(&'static str),
    PrivacyValidationRequired,
    ExtendedAcl,
    InvalidRevision,
    InvalidPortablePath,
    IdenticalPaths,
    CaseRenameContractRequired,
    InvalidCaseRenameContract,
    InvalidEntryName,
    EntryTooLarge,
    TooManyEntries,
    InvalidPageSize,
    UnsupportedVersion(u32),
    JournalCollision,
    IntentMismatch,
    ExternalMutation,
    PublishedButNotSynced(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "invalid journal JSON: {error}"),
            Self::InvalidRoot(reason) => write!(formatter, "invalid recovery root: {reason}"),
            Self::PrivacyValidationRequired => formatter.write_str(
                "recovery journal disabled: robust platform privacy validation is required",
            ),
            Self::ExtendedAcl => formatter.write_str("recovery journal object has an extended ACL"),
            Self::InvalidRevision => formatter.write_str("invalid BLAKE3 revision"),
            Self::InvalidPortablePath => formatter.write_str("invalid portable vault path"),
            Self::IdenticalPaths => formatter.write_str("rename source and destination are equal"),
            Self::CaseRenameContractRequired => {
                formatter.write_str("case-only rename requires the explicit temp contract")
            }
            Self::InvalidCaseRenameContract => {
                formatter.write_str("invalid case-only rename temp contract")
            }
            Self::InvalidEntryName => formatter.write_str("invalid journal entry name"),
            Self::EntryTooLarge => formatter.write_str("journal entry exceeds size limit"),
            Self::TooManyEntries => formatter.write_str("journal contains too many entries"),
            Self::InvalidPageSize => formatter.write_str("journal page size must be 1..=128"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported journal version {version}")
            }
            Self::JournalCollision => {
                formatter.write_str("operation id is already bound to a different intent")
            }
            Self::IntentMismatch => formatter.write_str("committed intent does not match expected"),
            Self::ExternalMutation => {
                formatter.write_str("journal topology was modified outside this operation")
            }
            Self::PublishedButNotSynced(error) => {
                write!(
                    formatter,
                    "journal published but directory sync failed: {error}"
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
    #[serde(default)]
    pub case_rename: bool,
}

impl RenameMoveIntent {
    pub const VERSION: u32 = 2;

    /// Creates a normal rename/move intent. Case-only renames must use
    /// [`Self::new_case_rename`]. Input paths are stored canonically.
    ///
    /// # Errors
    /// Returns an error for invalid paths, equal paths, collision-key aliases, or revisions.
    pub fn new(
        from: impl AsRef<str>,
        to: impl AsRef<str>,
        expected: FileRevision,
        temp: Option<String>,
    ) -> Result<Self, Error> {
        let from = canonical_portable(from.as_ref())?;
        let to = canonical_portable(to.as_ref())?;
        let temp = temp.map(|path| canonical_portable(&path)).transpose()?;
        validate_path_relationship(&from, &to, false, temp.as_deref())?;
        expected.validate()?;
        Ok(Self {
            version: Self::VERSION,
            operation_id: Uuid::new_v4(),
            from,
            to,
            expected,
            temp,
            case_rename: false,
        })
    }

    /// Creates an explicit two-step case-only rename intent.
    ///
    /// # Errors
    /// Returns an error unless source/destination differ exactly, share a collision key,
    /// and the temporary path has a distinct collision key from both.
    pub fn new_case_rename(
        from: impl AsRef<str>,
        to: impl AsRef<str>,
        expected: FileRevision,
        temp: impl AsRef<str>,
    ) -> Result<Self, Error> {
        let from = canonical_portable(from.as_ref())?;
        let to = canonical_portable(to.as_ref())?;
        let temp = canonical_portable(temp.as_ref())?;
        validate_path_relationship(&from, &to, true, Some(&temp))?;
        expected.validate()?;
        Ok(Self {
            version: Self::VERSION,
            operation_id: Uuid::new_v4(),
            from,
            to,
            expected,
            temp: Some(temp),
            case_rename: true,
        })
    }

    fn validate(&self) -> Result<(), Error> {
        if self.version != Self::VERSION {
            return Err(Error::UnsupportedVersion(self.version));
        }
        self.expected.validate()?;
        let from = canonical_portable(&self.from)?;
        let to = canonical_portable(&self.to)?;
        let temp = self.temp.as_deref().map(canonical_portable).transpose()?;
        if from != self.from || to != self.to || temp.as_deref() != self.temp.as_deref() {
            return Err(Error::InvalidPortablePath);
        }
        validate_path_relationship(&from, &to, self.case_rename, temp.as_deref())
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

/// Classifies observed topology conservatively. The journal is only an untrusted
/// hint; this function never mutates data or authorizes a mutation.
#[must_use]
pub fn decide_recovery(intent: &RenameMoveIntent, topology: &RecoveryTopology) -> RecoveryDecision {
    let expected = &intent.expected;
    match (&topology.from, &topology.to, &topology.temp) {
        (Some(from), None, None) if from == expected => RecoveryDecision::NotStarted,
        (None, None, Some(temp)) if temp == expected => RecoveryDecision::InProgressAtTemp,
        (None, Some(to), None) if to == expected => RecoveryDecision::Committed,
        (Some(from), Some(to), None) if from == expected && to == expected => {
            RecoveryDecision::DuplicateManual
        }
        (Some(from), Some(to), None) if from == expected && to != expected => {
            RecoveryDecision::DestinationCollision
        }
        (None, None, None) => RecoveryDecision::DataLoss,
        _ => RecoveryDecision::ExternalMutation,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Published,
    AlreadyPublished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompleteOutcome {
    Completed,
    AlreadyCompleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalPage {
    pub entries: Vec<RenameMoveIntent>,
    pub next_after: Option<Uuid>,
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionTombstone {
    version: u32,
    operation_id: Uuid,
    intent_blake3_hex: String,
}

impl CompletionTombstone {
    const VERSION: u32 = 1;

    fn for_intent(intent: &RenameMoveIntent) -> Result<Self, Error> {
        Ok(Self {
            version: Self::VERSION,
            operation_id: intent.operation_id,
            intent_blake3_hex: blake3::hash(&canonical_bytes(intent)?).to_hex().to_string(),
        })
    }

    fn validate_for(&self, intent: &RenameMoveIntent) -> Result<(), Error> {
        let expected = Self::for_intent(intent)?;
        if self == &expected {
            Ok(())
        } else {
            Err(Error::IntentMismatch)
        }
    }
}

/// Append-only recovery evidence. Physical retention/garbage collection is
/// deliberately deferred; completion is represented only by tombstones.
pub struct RecoveryJournal {
    directory: Dir,
    completed: Dir,
}

impl RecoveryJournal {
    /// Opens a journal below a private app-data root. Both roots are opened
    /// without following symlinks and their identities are stabilized before
    /// the disjointness decision.
    ///
    /// # Errors
    /// Returns an error for unstable, symlinked, overlapping, or non-private roots.
    pub fn open(app_data_root: &Path, vault_root: &Path) -> Result<Self, Error> {
        #[cfg(windows)]
        {
            let _ = (app_data_root, vault_root);
            return Err(Error::PrivacyValidationRequired);
        }

        #[cfg(not(windows))]
        {
            let app_before = app_data_root.canonicalize()?;
            let vault_before = vault_root.canonicalize()?;
            let app_directory = open_absolute_dir_nofollow(app_data_root)?;
            let vault_directory = open_absolute_dir_nofollow(vault_root)?;
            let app_after = app_data_root.canonicalize()?;
            let vault_after = vault_root.canonicalize()?;
            if app_before != app_after || vault_before != vault_after {
                return Err(Error::InvalidRoot("root changed while it was opened"));
            }
            verify_root_identity(&app_directory, &app_after)?;
            verify_root_identity(&vault_directory, &vault_after)?;
            validate_disjoint_canonical(&app_after, &vault_after)?;
            require_private_directory(&app_directory)?;

            let created = match app_directory.create_dir(JOURNAL_DIRECTORY) {
                Ok(()) => true,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
                Err(error) => return Err(error.into()),
            };
            let directory = open_child_dir_nofollow(&app_directory, JOURNAL_DIRECTORY)?;
            if created {
                set_held_directory_permissions(&directory)?;
                sync_held_directory(&app_directory)?;
            }
            require_private_directory(&directory)?;
            let completed_created = match directory.create_dir(COMPLETED_DIRECTORY) {
                Ok(()) => true,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
                Err(error) => return Err(error.into()),
            };
            let completed = open_child_dir_nofollow(&directory, COMPLETED_DIRECTORY)?;
            if completed_created {
                set_held_directory_permissions(&completed)?;
                sync_held_directory(&directory)?;
            }
            require_private_directory(&completed)?;
            Ok(Self {
                directory,
                completed,
            })
        }
    }

    /// Durably publishes an intent using a fresh temp and atomic no-replace rename.
    /// Stale temps are immutable crash evidence and are never removed or reused.
    ///
    /// # Errors
    /// Fails closed on collisions, unexpected topology, insecure files, or I/O errors.
    pub fn publish(&self, intent: &RenameMoveIntent) -> Result<PublishOutcome, Error> {
        intent.validate()?;
        let bytes = canonical_bytes(intent)?;
        let final_name = entry_name(intent.operation_id);
        if let Some(actual) = Self::read_raw_if_exists(&self.directory, &final_name)? {
            return compare_published(&actual, &bytes);
        }
        if Self::publish_bytes(&self.directory, &final_name, &bytes)? {
            Ok(PublishOutcome::Published)
        } else {
            Ok(PublishOutcome::AlreadyPublished)
        }
    }

    /// Publishes an immutable completion tombstone after verifying the original
    /// journal bytes. Neither the journal nor stale temp evidence is deleted.
    ///
    /// # Errors
    /// Fails on mismatched intent, unexpected temp files, insecure files, or I/O errors.
    pub fn complete(
        &self,
        operation_id: Uuid,
        expected_intent: &RenameMoveIntent,
    ) -> Result<CompleteOutcome, Error> {
        expected_intent.validate()?;
        if expected_intent.operation_id != operation_id {
            return Err(Error::IntentMismatch);
        }
        let final_name = entry_name(operation_id);
        let Some(actual) = Self::read_raw_if_exists(&self.directory, &final_name)? else {
            return Err(Error::IntentMismatch);
        };
        if actual != canonical_bytes(expected_intent)? {
            return Err(Error::IntentMismatch);
        }
        let tombstone = CompletionTombstone::for_intent(expected_intent)?;
        let tombstone_bytes = serde_json::to_vec(&tombstone)?;
        let tombstone_name = entry_name(operation_id);
        if let Some(actual) = Self::read_raw_if_exists(&self.completed, &tombstone_name)? {
            let observed: CompletionTombstone = serde_json::from_slice(&actual)?;
            observed.validate_for(expected_intent)?;
            if serde_json::to_vec(&observed)? != actual {
                return Err(Error::IntentMismatch);
            }
            return Ok(CompleteOutcome::AlreadyCompleted);
        }
        if Self::publish_bytes(&self.completed, &tombstone_name, &tombstone_bytes)? {
            Ok(CompleteOutcome::Completed)
        } else {
            Ok(CompleteOutcome::AlreadyCompleted)
        }
    }

    /// Reads and validates one bounded, private journal entry.
    ///
    /// # Errors
    /// Returns an error for an absent, malformed, oversized, insecure, or mismatched entry.
    pub fn read(&self, operation_id: Uuid) -> Result<RenameMoveIntent, Error> {
        let bytes = Self::read_raw(&self.directory, &entry_name(operation_id))?;
        let intent: RenameMoveIntent = serde_json::from_slice(&bytes)?;
        intent.validate()?;
        if intent.operation_id != operation_id || canonical_bytes(&intent)? != bytes {
            return Err(Error::InvalidEntryName);
        }
        Ok(intent)
    }

    /// Lists a deterministic page of logically active entries. Only an exact,
    /// valid completion tombstone suppresses an entry. Journal records and stale
    /// temps are physically retained; bounded garbage collection is deferred.
    ///
    /// # Errors
    /// Returns an error for invalid limits, excessive committed entries, or invalid entries.
    pub fn list_page(&self, after: Option<Uuid>, limit: usize) -> Result<JournalPage, Error> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(Error::InvalidPageSize);
        }
        let ids = self.active_ids()?;
        let mut selected = ids
            .into_iter()
            .filter(|id| after.is_none_or(|cursor| *id > cursor))
            .take(limit + 1)
            .collect::<Vec<_>>();
        let has_more = selected.len() > limit;
        selected.truncate(limit);
        let next_after = if has_more {
            selected.last().copied()
        } else {
            None
        };
        let entries = selected
            .into_iter()
            .map(|id| self.read(id))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(JournalPage {
            entries,
            next_after,
        })
    }

    fn active_ids(&self) -> Result<Vec<Uuid>, Error> {
        let mut ids = Vec::new();
        for entry in self.directory.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = parse_entry_name(name) else {
                continue;
            };
            if self.has_valid_completion(id)? {
                continue;
            }
            ids.push(id);
            if ids.len() > MAX_ENTRY_COUNT {
                return Err(Error::TooManyEntries);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn has_valid_completion(&self, operation_id: Uuid) -> Result<bool, Error> {
        let intent = self.read(operation_id)?;
        let name = entry_name(operation_id);
        let Ok(Some(bytes)) = Self::read_raw_if_exists(&self.completed, &name) else {
            return Ok(false);
        };
        let Ok(tombstone) = serde_json::from_slice::<CompletionTombstone>(&bytes) else {
            return Ok(false);
        };
        Ok(tombstone.validate_for(&intent).is_ok()
            && serde_json::to_vec(&tombstone).is_ok_and(|canonical| canonical == bytes))
    }

    fn read_raw_if_exists(directory: &Dir, name: &str) -> Result<Option<Vec<u8>>, Error> {
        match Self::read_raw(directory, name) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn read_raw(directory: &Dir, name: &str) -> Result<Vec<u8>, Error> {
        let metadata = directory.symlink_metadata(name)?;
        if !metadata.file_type().is_file() {
            return Err(Error::InvalidEntryName);
        }
        if metadata.len() > MAX_ENTRY_BYTES {
            return Err(Error::EntryTooLarge);
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = directory.open_with(name, &options)?;
        verify_private_file(&file, 1)?;
        let capacity = usize::try_from(metadata.len()).map_err(|_| Error::EntryTooLarge)?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Err(Error::EntryTooLarge);
        }
        Ok(bytes)
    }

    fn publish_bytes(directory: &Dir, final_name: &str, bytes: &[u8]) -> Result<bool, Error> {
        let temporary_name = format!(".publish-{}.tmp", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = directory.open_with(&temporary_name, &options)?;
        set_held_file_permissions(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        verify_private_file(&file, 1)?;
        drop(file);
        match atomic_rename_noreplace(directory, &temporary_name, final_name) {
            Ok(()) => {
                sync_held_directory(directory).map_err(published_sync_error)?;
                Ok(true)
            }
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                let actual = Self::read_raw(directory, final_name)?;
                if actual == bytes {
                    Ok(false)
                } else {
                    Err(Error::JournalCollision)
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn canonical_portable(path: &str) -> Result<String, Error> {
    VaultPath::from_portable(path)
        .map(|path| path.as_str().to_owned())
        .map_err(|_| Error::InvalidPortablePath)
}

fn validate_path_relationship(
    from: &str,
    to: &str,
    case_rename: bool,
    temp: Option<&str>,
) -> Result<(), Error> {
    if from == to {
        return Err(Error::IdenticalPaths);
    }
    let from_path = VaultPath::from_portable(from).map_err(|_| Error::InvalidPortablePath)?;
    let to_path = VaultPath::from_portable(to).map_err(|_| Error::InvalidPortablePath)?;
    let same_key = from_path.collision_key() == to_path.collision_key();
    if same_key && !case_rename {
        return Err(Error::CaseRenameContractRequired);
    }
    if case_rename {
        let Some(temp) = temp else {
            return Err(Error::InvalidCaseRenameContract);
        };
        let temp_path = VaultPath::from_portable(temp).map_err(|_| Error::InvalidPortablePath)?;
        if !same_key
            || temp_path.collision_key() == from_path.collision_key()
            || temp_path.collision_key() == to_path.collision_key()
        {
            return Err(Error::InvalidCaseRenameContract);
        }
    } else if let Some(temp) = temp {
        let temp_path = VaultPath::from_portable(temp).map_err(|_| Error::InvalidPortablePath)?;
        if temp_path.collision_key() == from_path.collision_key()
            || temp_path.collision_key() == to_path.collision_key()
        {
            return Err(Error::InvalidCaseRenameContract);
        }
    }
    Ok(())
}

fn canonical_bytes(intent: &RenameMoveIntent) -> Result<Vec<u8>, Error> {
    let bytes = serde_json::to_vec(intent)?;
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        return Err(Error::EntryTooLarge);
    }
    Ok(bytes)
}

fn compare_published(actual: &[u8], expected: &[u8]) -> Result<PublishOutcome, Error> {
    if actual == expected {
        Ok(PublishOutcome::AlreadyPublished)
    } else {
        Err(Error::JournalCollision)
    }
}

fn entry_name(operation_id: Uuid) -> String {
    format!("{operation_id}.json")
}

fn parse_entry_name(name: &str) -> Option<Uuid> {
    let id_text = name.strip_suffix(".json")?;
    let id = Uuid::parse_str(id_text).ok()?;
    (id.to_string() == id_text).then_some(id)
}

#[cfg(not(windows))]
fn validate_disjoint_canonical(app: &Path, vault: &Path) -> Result<(), Error> {
    if app == vault || app.starts_with(vault) || vault.starts_with(app) {
        return Err(Error::InvalidRoot(
            "app data and vault roots must be disjoint",
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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
fn verify_root_identity(directory: &Dir, canonical: &Path) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;
    let held = directory.try_clone()?.into_std_file().metadata()?;
    let ambient = fs::metadata(canonical)?;
    if held.dev() != ambient.dev() || held.ino() != ambient.ino() {
        return Err(Error::InvalidRoot(
            "root identity changed while it was opened",
        ));
    }
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn verify_root_identity(_directory: &Dir, _canonical: &Path) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(unix)]
fn require_private_directory(directory: &Dir) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;
    let held = directory.try_clone()?.into_std_file();
    let metadata = held.metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(Error::InvalidRoot(
            "private directory is not owned by current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(Error::InvalidRoot(
            "private directory grants group or world access",
        ));
    }
    verify_no_extended_acl(&held)?;
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn require_private_directory(_directory: &Dir) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
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

#[cfg(all(not(unix), not(windows)))]
fn set_held_directory_permissions(_directory: &Dir) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(unix)]
fn set_held_file_permissions(file: &cap_std::fs::File) -> Result<(), Error> {
    use cap_std::fs::{Permissions, PermissionsExt};
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_held_file_permissions(_file: &cap_std::fs::File) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(unix)]
fn verify_private_file(file: &cap_std::fs::File, max_links: u64) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;
    let held = file.try_clone()?.into_std();
    let metadata = held.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || !(1..=max_links).contains(&metadata.nlink())
    {
        return Err(Error::ExternalMutation);
    }
    verify_no_extended_acl(&held)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_no_extended_acl(file: &std::fs::File) -> Result<(), Error> {
    if myvault_platform_acl::has_extended_acl(file)? {
        return Err(Error::ExtendedAcl);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_no_extended_acl(file: &std::fs::File) -> Result<(), Error> {
    use xattr::FileExt;

    if file.get_xattr("system.posix_acl_access")?.is_some()
        || file.get_xattr("system.posix_acl_default")?.is_some()
    {
        return Err(Error::ExtendedAcl);
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn verify_no_extended_acl(_file: &std::fs::File) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(not(unix))]
fn verify_private_file(_file: &cap_std::fs::File, _max_links: u64) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

fn sync_held_directory(directory: &Dir) -> Result<(), Error> {
    directory.try_clone()?.into_std_file().sync_all()?;
    Ok(())
}

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
fn atomic_rename_noreplace(directory: &Dir, source: &str, destination: &str) -> Result<(), Error> {
    let held = directory.try_clone()?.into_std_file();
    rustix::fs::renameat_with(
        &held,
        source,
        &held,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| Error::Io(io::Error::from(error)))
}

#[cfg(not(any(target_os = "android", target_os = "linux", target_os = "macos")))]
fn atomic_rename_noreplace(
    _directory: &Dir,
    _source: &str,
    _destination: &str,
) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}
