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
    PublishedCleanupFailed(io::Error),
    CompletedButNotSynced(io::Error),
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
            Self::PublishedCleanupFailed(error) => {
                write!(
                    formatter,
                    "journal published but temp cleanup failed: {error}"
                )
            }
            Self::CompletedButNotSynced(error) => {
                write!(
                    formatter,
                    "journal removed but directory sync failed: {error}"
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
    ReconciledAfterTempWrite,
    AlreadyPublished,
    AlreadyPublishedAndCleanedTemp,
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

pub struct RecoveryJournal {
    directory: Dir,
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
            Ok(Self { directory })
        }
    }

    /// Durably publishes or reconciles an intent under deterministic temp/final names.
    ///
    /// # Errors
    /// Fails closed on collisions, unexpected topology, insecure files, or I/O errors.
    pub fn publish(&self, intent: &RenameMoveIntent) -> Result<PublishOutcome, Error> {
        intent.validate()?;
        let bytes = canonical_bytes(intent)?;
        let final_name = entry_name(intent.operation_id);
        let temporary_name = temporary_entry_name(intent.operation_id);

        loop {
            let final_present = self.name_exists(&final_name)?;
            let temp_present = self.name_exists(&temporary_name)?;
            let final_observed = if final_present {
                Some(if temp_present {
                    self.read_raw_allow_recovery_link(&final_name)?
                } else {
                    self.read_raw(&final_name)?
                })
            } else {
                None
            };
            let temp_observed = if temp_present {
                Some(if final_present {
                    self.read_raw_allow_recovery_link(&temporary_name)?
                } else {
                    self.read_raw(&temporary_name)?
                })
            } else {
                None
            };
            match (final_observed, temp_observed) {
                (Some(final_bytes), None) => return compare_published(&final_bytes, &bytes),
                (Some(final_bytes), Some(temp_bytes)) => {
                    if final_bytes != bytes {
                        if temp_bytes != bytes {
                            return Err(Error::ExternalMutation);
                        }
                        self.remove_temp_and_sync(&temporary_name)?;
                        return Err(Error::JournalCollision);
                    }
                    if temp_bytes != bytes {
                        return Err(Error::ExternalMutation);
                    }
                    self.remove_temp_and_sync(&temporary_name)?;
                    return Ok(PublishOutcome::AlreadyPublishedAndCleanedTemp);
                }
                (None, Some(temp_bytes)) => {
                    if temp_bytes != bytes {
                        self.remove_verified_orphan_temp(&temporary_name)?;
                        continue;
                    }
                    return self.link_sync_cleanup(
                        &temporary_name,
                        &final_name,
                        &bytes,
                        true,
                        false,
                    );
                }
                (None, None) => {}
            }

            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            let mut file = match self.directory.open_with(&temporary_name, &options) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let observed = self
                        .read_raw_if_exists(&temporary_name)?
                        .ok_or(Error::ExternalMutation)?;
                    if observed != bytes {
                        self.remove_verified_orphan_temp(&temporary_name)?;
                        continue;
                    }
                    return self.link_sync_cleanup(
                        &temporary_name,
                        &final_name,
                        &bytes,
                        true,
                        false,
                    );
                }
                Err(error) => return Err(error.into()),
            };
            set_held_file_permissions(&file)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            verify_private_file(&file, 1)?;
            drop(file);
            return self.link_sync_cleanup(&temporary_name, &final_name, &bytes, false, true);
        }
    }

    /// Removes a committed intent only when it exactly matches the caller's
    /// independently-held expected intent.
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
        let temporary_name = temporary_entry_name(operation_id);
        if self.read_raw_if_exists(&temporary_name)?.is_some() {
            return Err(Error::ExternalMutation);
        }
        let Some(actual) = self.read_raw_if_exists(&final_name)? else {
            return Ok(CompleteOutcome::AlreadyCompleted);
        };
        if actual != canonical_bytes(expected_intent)? {
            return Err(Error::IntentMismatch);
        }
        self.directory.remove_file(&final_name)?;
        sync_held_directory(&self.directory).map_err(completed_sync_error)?;
        Ok(CompleteOutcome::Completed)
    }

    /// Reads and validates one bounded, private journal entry.
    ///
    /// # Errors
    /// Returns an error for an absent, malformed, oversized, insecure, or mismatched entry.
    pub fn read(&self, operation_id: Uuid) -> Result<RenameMoveIntent, Error> {
        let bytes = self.read_raw(&entry_name(operation_id))?;
        let intent: RenameMoveIntent = serde_json::from_slice(&bytes)?;
        intent.validate()?;
        if intent.operation_id != operation_id || canonical_bytes(&intent)? != bytes {
            return Err(Error::InvalidEntryName);
        }
        Ok(intent)
    }

    /// Lists a deterministic page of committed entries. Junk and temp names do
    /// not count toward the committed-entry limit.
    ///
    /// # Errors
    /// Returns an error for invalid limits, excessive committed entries, or invalid entries.
    pub fn list_page(&self, after: Option<Uuid>, limit: usize) -> Result<JournalPage, Error> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(Error::InvalidPageSize);
        }
        let ids = self.committed_ids()?;
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

    fn committed_ids(&self) -> Result<Vec<Uuid>, Error> {
        let mut ids = Vec::new();
        for entry in self.directory.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = parse_entry_name(name) else {
                continue;
            };
            ids.push(id);
            if ids.len() > MAX_ENTRY_COUNT {
                return Err(Error::TooManyEntries);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn read_raw_if_exists(&self, name: &str) -> Result<Option<Vec<u8>>, Error> {
        match self.read_raw(name) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn name_exists(&self, name: &str) -> Result<bool, Error> {
        match self.directory.symlink_metadata(name) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn read_raw(&self, name: &str) -> Result<Vec<u8>, Error> {
        self.read_raw_with_link_limit(name, 1)
    }

    fn read_raw_allow_recovery_link(&self, name: &str) -> Result<Vec<u8>, Error> {
        self.read_raw_with_link_limit(name, 2)
    }

    fn read_raw_with_link_limit(&self, name: &str, max_links: u64) -> Result<Vec<u8>, Error> {
        let metadata = self.directory.symlink_metadata(name)?;
        if !metadata.file_type().is_file() {
            return Err(Error::InvalidEntryName);
        }
        if metadata.len() > MAX_ENTRY_BYTES {
            return Err(Error::EntryTooLarge);
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = self.directory.open_with(name, &options)?;
        verify_private_file(&file, max_links)?;
        let capacity = usize::try_from(metadata.len()).map_err(|_| Error::EntryTooLarge)?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Err(Error::EntryTooLarge);
        }
        Ok(bytes)
    }

    fn link_sync_cleanup(
        &self,
        temporary_name: &str,
        final_name: &str,
        expected_bytes: &[u8],
        recovered: bool,
        owned_temp: bool,
    ) -> Result<PublishOutcome, Error> {
        match self
            .directory
            .hard_link(temporary_name, &self.directory, final_name)
        {
            Ok(()) => {
                sync_held_directory(&self.directory).map_err(published_sync_error)?;
                self.remove_temp_and_sync(temporary_name)?;
                let final_bytes = self.read_raw(final_name)?;
                if final_bytes != expected_bytes {
                    return Err(Error::ExternalMutation);
                }
                Ok(if recovered {
                    PublishOutcome::ReconciledAfterTempWrite
                } else {
                    PublishOutcome::Published
                })
            }
            Err(link_error) => {
                let temp_bytes = self.read_raw_if_exists(temporary_name)?;
                if owned_temp && temp_bytes.as_deref() != Some(expected_bytes) {
                    return Err(Error::ExternalMutation);
                }
                if temp_bytes.as_deref() == Some(expected_bytes) {
                    self.remove_temp_and_sync(temporary_name)?;
                }
                let final_bytes = self.read_raw_if_exists(final_name)?;
                match final_bytes {
                    Some(bytes) if bytes == expected_bytes => {
                        Ok(PublishOutcome::AlreadyPublishedAndCleanedTemp)
                    }
                    Some(_) => Err(Error::JournalCollision),
                    None => Err(Error::Io(link_error)),
                }
            }
        }
    }

    fn remove_temp_and_sync(&self, temporary_name: &str) -> Result<(), Error> {
        self.directory
            .remove_file(temporary_name)
            .map_err(Error::PublishedCleanupFailed)?;
        sync_held_directory(&self.directory).map_err(published_sync_error)
    }

    /// Removes only a canonical, private, single-link crash temp while the final
    /// name is absent. The held descriptor is verified before and after reading;
    /// symlinks, hardlinks, insecure files, or a concurrently-created final fail closed.
    fn remove_verified_orphan_temp(&self, temporary_name: &str) -> Result<(), Error> {
        let Some(operation_id) = parse_temporary_entry_name(temporary_name) else {
            return Err(Error::InvalidEntryName);
        };
        if temporary_entry_name(operation_id) != temporary_name {
            return Err(Error::InvalidEntryName);
        }
        let final_name = entry_name(operation_id);
        if self.name_exists(&final_name)? {
            return Err(Error::JournalCollision);
        }

        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = self.directory.open_with(temporary_name, &options)?;
        verify_private_file(&file, 1)?;
        let held_before = file_identity(&file)?;
        if self.name_exists(&final_name)? {
            return Err(Error::JournalCollision);
        }
        let named = self.directory.symlink_metadata(temporary_name)?;
        if !named.file_type().is_file() || file_identity_from_metadata(&named) != held_before {
            return Err(Error::ExternalMutation);
        }
        self.directory
            .remove_file(temporary_name)
            .map_err(Error::PublishedCleanupFailed)?;
        sync_held_directory(&self.directory).map_err(published_sync_error)
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

fn temporary_entry_name(operation_id: Uuid) -> String {
    format!(".{operation_id}.json.tmp")
}

fn parse_temporary_entry_name(name: &str) -> Option<Uuid> {
    let id_text = name.strip_prefix('.')?.strip_suffix(".json.tmp")?;
    let id = Uuid::parse_str(id_text).ok()?;
    (id.to_string() == id_text).then_some(id)
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

fn completed_sync_error(error: Error) -> Error {
    match error {
        Error::Io(error) => Error::CompletedButNotSynced(error),
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

#[cfg(unix)]
fn file_identity(file: &cap_std::fs::File) -> Result<(u64, u64), Error> {
    use cap_fs_ext::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn file_identity_from_metadata(metadata: &cap_std::fs::Metadata) -> (u64, u64) {
    use cap_fs_ext::MetadataExt;
    (metadata.dev(), metadata.ino())
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
