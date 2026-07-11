use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsExt;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};

use crate::atomic_move::rename_noreplace;
use crate::capability::{open_absolute_dir_nofollow, open_child_dir_nofollow};
use crate::path::{classify_component, component_collision_key, VaultPathClass};
use crate::{CoreError, FileRevision, Result, TrashArea, TrashEntryKind, TrashPath, VaultPath};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
static MUTATION_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
pub const DEFAULT_READ_LIMIT: usize = 16 * 1024 * 1024;
pub const MAX_TRASH_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
/// In-process vault instances share a mutation lock, but another process can
/// still alter the filesystem between descriptor-relative validation and commit.
pub const MUTATION_EXTERNAL_PROCESS_RESIDUAL_RISK: &str =
    "external processes are not serialized with myVault's per-root mutation lock";
/// Revision checks and moves share the in-process root lock, but another process
/// can still change an opened file between verification and the atomic rename.
pub const TRASH_REVISION_EXTERNAL_PROCESS_RESIDUAL_RISK: &str =
    "an external process can mutate a payload between descriptor-relative revision verification and rename";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InventoryLimits {
    pub max_depth: usize,
    pub max_entries: usize,
}

impl Default for InventoryLimits {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_entries: 100_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventoryKind {
    Markdown,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryEntry {
    pub path: VaultPath,
    pub kind: InventoryKind,
    pub size: u64,
}

/// Identifies whether a write was explicitly initiated by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteIntent {
    Automatic,
    UserInitiated,
}

/// Durability reached after an atomic move has been published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveDurability {
    FullySynced,
    /// Windows accepted the move but its filesystem does not permit flushing
    /// one or both directory handles.
    DirectorySyncUnsupported,
}

/// Result of flushing one parent directory after a published move.
#[derive(Debug)]
pub enum DirectorySyncStatus {
    Synced,
    Unsupported,
    Failed(std::io::Error),
    SharedWithDestination,
    NotAttempted,
}

impl DirectorySyncStatus {
    pub(crate) fn error(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Failed(error) => Some(error),
            Self::Synced | Self::Unsupported | Self::SharedWithDestination | Self::NotAttempted => {
                None
            }
        }
    }
}

impl std::fmt::Display for DirectorySyncStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Synced => formatter.write_str("synced"),
            Self::Unsupported => formatter.write_str("unsupported"),
            Self::Failed(error) => write!(formatter, "failed: {error}"),
            Self::SharedWithDestination => {
                formatter.write_str("same parent; destination result applies")
            }
            Self::NotAttempted => formatter.write_str("not attempted"),
        }
    }
}

/// A vault whose filesystem authority is held by an open directory handle.
#[derive(Debug)]
pub struct Vault {
    root_path: PathBuf,
    root_dir: Dir,
    mutation_lock: Arc<Mutex<()>>,
}

impl Vault {
    /// Opens an existing vault without following any symlink component.
    ///
    /// # Errors
    ///
    /// Returns an error when the root is not an absolute accessible directory
    /// or any component is a symlink.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let supplied = root.as_ref();
        let root_dir = open_absolute_dir_nofollow(supplied)?;
        let root_path = std::fs::canonicalize(supplied)?;
        let mutation_lock = shared_mutation_lock(&root_path)?;
        Ok(Self {
            root_path,
            root_dir,
            mutation_lock,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root_path
    }

    /// Reads a file relative to the held vault capability without following links.
    ///
    /// # Errors
    ///
    /// Returns an error when a parent or destination is a symlink, is missing,
    /// or cannot be read.
    pub fn read(&self, relative: &VaultPath) -> Result<Vec<u8>> {
        self.read_bounded(relative, DEFAULT_READ_LIMIT)
    }

    /// Reads at most `limit` bytes, rejecting a larger file rather than
    /// allocating in proportion to untrusted filesystem content.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, filesystem failures, or when the
    /// content exceeds `limit`.
    pub fn read_bounded(&self, relative: &VaultPath, limit: usize) -> Result<Vec<u8>> {
        Self::validate_generic_access(relative)?;
        let (parent, name) = self.open_parent(relative)?;
        self.reject_final_symlink(&parent, &name, relative)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = parent.open_with(&name, &options)?;
        let metadata = file.metadata()?;
        if metadata.len() > limit as u64 {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "file size",
                limit,
            });
        }
        let capacity = usize::try_from(metadata.len()).unwrap_or(limit).min(limit);
        let mut bytes = Vec::with_capacity(capacity);
        Read::by_ref(&mut file)
            .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "file size",
                limit,
            });
        }
        Ok(bytes)
    }

    /// Computes a descriptor-relative, no-follow BLAKE3 revision while reading
    /// no more than `max_bytes + 1` bytes.
    ///
    /// # Errors
    /// Returns an error for internal trash paths, non-files, symlinks, unsafe
    /// components, I/O failures, or content larger than `max_bytes`.
    pub fn revision(&self, relative: &VaultPath, max_bytes: usize) -> Result<FileRevision> {
        Self::validate_generic_access(relative)?;
        self.revision_inner(relative, max_bytes)
    }

    /// Verifies a file against a caller-held expected revision.
    ///
    /// # Errors
    /// Returns [`CoreError::StaleRevision`] on a digest or byte-length mismatch,
    /// or another bounded/no-follow revision error.
    pub fn verify_expected(
        &self,
        relative: &VaultPath,
        expected: &FileRevision,
        max_bytes: usize,
    ) -> Result<()> {
        Self::validate_generic_access(relative)?;
        self.verify_expected_inner(relative, expected, max_bytes)
    }

    /// Inventories regular vault files without following symbolic links.
    /// Internal `.obsidian` and `.trash` trees are excluded entirely.
    ///
    /// # Errors
    ///
    /// Returns an error for symlinks, invalid portable names, filesystem
    /// failures, or an exceeded traversal limit.
    pub fn inventory(&self, limits: InventoryLimits) -> Result<Vec<InventoryEntry>> {
        let mut output = Vec::new();
        let mut visited = 0_usize;
        self.inventory_dir(&self.root_dir, &[], 0, limits, &mut visited, &mut output)?;
        output.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let mut collision_keys = std::collections::HashMap::with_capacity(output.len());
        for entry in &output {
            let key = entry.path.collision_key();
            if let Some(existing) = collision_keys.insert(key, entry.path.as_str()) {
                return Err(CoreError::PortablePathCollision {
                    existing: existing.to_owned(),
                    incoming: entry.path.as_str().to_owned(),
                });
            }
        }
        Ok(output)
    }

    /// Creates every missing directory component without following links.
    ///
    /// # Errors
    ///
    /// Returns an error for a denied internal path, symlink/non-directory
    /// component, portable-name collision, or filesystem failure.
    pub fn create_directories(&self, relative: &VaultPath, intent: WriteIntent) -> Result<()> {
        let _guard = self.lock_mutations()?;
        Self::validate_mutation_policy(relative, intent)?;
        self.create_directories_inner(relative)
    }

    fn create_directories_inner(&self, relative: &VaultPath) -> Result<()> {
        let mut current = self.root_dir.try_clone()?;
        let mut display = self.root_path.clone();
        for component in relative.as_path().components() {
            let name = component.as_os_str();
            let name_utf8 = name
                .to_str()
                .ok_or_else(|| CoreError::InvalidRelativePath(relative.as_path().to_owned()))?;
            self.reject_sibling_collision(&current, name_utf8, relative)?;
            display.push(name);
            match current.create_dir(name) {
                Ok(()) => sync_directory(&current)?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            current = open_child_dir_nofollow(&current, name, &display)?;
        }
        Ok(())
    }

    /// Crash-safely creates a file and fails if the destination already exists.
    /// The final hard-link publication has create-new/no-replace semantics.
    ///
    /// # Errors
    ///
    /// Returns an error when the destination exists, a path is unsafe, hard
    /// links are unsupported by the filesystem, or another I/O operation fails.
    pub fn create_new(
        &self,
        relative: &VaultPath,
        contents: &[u8],
        intent: WriteIntent,
    ) -> Result<()> {
        let _guard = self.lock_mutations()?;
        self.create_new_inner(relative, contents, intent, |_| Ok(()))
    }

    fn create_new_inner<F>(
        &self,
        relative: &VaultPath,
        contents: &[u8],
        intent: WriteIntent,
        mut inject: F,
    ) -> Result<()>
    where
        F: FnMut(CreateStage) -> std::io::Result<()>,
    {
        Self::validate_mutation_policy(relative, intent)?;
        let components: Vec<_> = relative.as_path().components().collect();
        if components.len() > 1 {
            let parent_path = components[..components.len() - 1]
                .iter()
                .map(|component| component.as_os_str())
                .collect::<PathBuf>();
            self.create_directories_inner(&VaultPath::new(parent_path)?)?;
        }
        let (parent, destination_name) = self.open_parent(relative)?;
        let destination_utf8 = destination_name
            .to_str()
            .ok_or_else(|| CoreError::InvalidRelativePath(relative.as_path().to_owned()))?;
        self.reject_sibling_collision(&parent, destination_utf8, relative)?;
        self.reject_final_symlink(&parent, &destination_name, relative)?;
        let (temp_name, mut file) = Self::create_temp(&parent)?;
        let prepublication = (|| {
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);
            parent.hard_link(&temp_name, &parent, &destination_name)?;
            Ok::<(), CoreError>(())
        })();
        if let Err(error) = prepublication {
            let _ = parent.remove_file(&temp_name);
            return Err(error);
        }

        let path = relative.as_path().to_owned();
        if let Err(source) = inject(CreateStage::LinkPublished) {
            return Err(CoreError::CommitOutcomeUnknown { path, source });
        }
        if let Err(error) = sync_directory(&parent) {
            return Err(Self::commit_unknown(path, error));
        }
        if let Err(source) = inject(CreateStage::DirectorySynced) {
            return Err(Self::cleanup_pending(path, &temp_name, source));
        }
        if let Err(source) = parent.remove_file(&temp_name) {
            return Err(Self::cleanup_pending(path, &temp_name, source));
        }
        if let Err(source) = inject(CreateStage::TempRemoved) {
            return Err(Self::cleanup_pending(path, &temp_name, source));
        }
        sync_directory(&parent)
            .map_err(|error| Self::cleanup_pending_from_core(path, &temp_name, error))
    }

    /// Atomically replaces a file through an already-open parent capability.
    ///
    /// The destination parent must already exist. The temporary file is
    /// flushed and renamed relative to the same directory handle, so replacing
    /// the path to that directory with a symlink cannot redirect the commit.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem failures, symlink components, portable
    /// collisions, generic `.trash` writes, or automatic `.obsidian` writes.
    pub fn atomic_write(
        &self,
        relative: &VaultPath,
        contents: &[u8],
        intent: WriteIntent,
    ) -> Result<()> {
        let _guard = self.lock_mutations()?;
        self.atomic_write_inner(relative, contents, intent, || {})
    }

    /// Atomically moves a file or directory and never replaces a destination.
    ///
    /// Both parents are opened before publication and the rename is resolved
    /// relative to those held capabilities. Destination and then source parent
    /// directories are flushed; a shared parent is flushed once.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::AlreadyExists`] when the destination exists,
    /// [`CoreError::AtomicNoReplaceUnsupported`] when the host filesystem lacks
    /// the required atomic primitive, or another safety/filesystem error.
    pub fn atomic_move(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        intent: WriteIntent,
    ) -> Result<MoveDurability> {
        self.atomic_move_inner(source, destination, intent, |_, directory| {
            sync_directory_for_move(directory)
        })
    }

    /// Moves one verified content file into an exact staging payload path.
    /// Generic APIs remain denied access to `.trash`.
    ///
    /// # Errors
    /// Returns an error for stale revisions, non-files, wrong trash layout,
    /// symlinks, collisions, unsupported atomic moves, or durability failures.
    pub fn move_content_to_trash_payload(
        &self,
        source: &VaultPath,
        staging_payload: &TrashPath,
        expected: &FileRevision,
    ) -> Result<MoveDurability> {
        let _guard = self.lock_mutations()?;
        Self::require_content_path(source)?;
        Self::require_trash_payload(staging_payload, TrashArea::Staging)?;
        self.verify_expected_inner(source, expected, MAX_TRASH_PAYLOAD_BYTES)?;
        let destination = staging_payload.as_vault_path();
        let durability = match self.atomic_move_locked(source, destination, |_, directory| {
            sync_directory_for_move(directory)
        }) {
            Ok(durability) => durability,
            Err(CoreError::AtomicMoveOutcomeUnknown { .. }) => self.confirm_move_durability(
                source,
                destination,
                expected,
                MAX_TRASH_PAYLOAD_BYTES,
            )?,
            Err(error) => return Err(error),
        };
        self.verify_published_topology(source, destination, expected, MAX_TRASH_PAYLOAD_BYTES)
            .map_err(|verification| {
                Self::verified_move_unknown_after_success(
                    source,
                    destination,
                    durability,
                    verification,
                )
            })?;
        Ok(durability)
    }

    /// Restores one verified committed trash payload to a content path.
    ///
    /// # Errors
    /// Returns an error for stale revisions, non-files, wrong trash layout,
    /// symlinks, collisions, unsupported atomic moves, or durability failures.
    pub fn restore_trash_payload(
        &self,
        payload: &TrashPath,
        destination: &VaultPath,
        expected: &FileRevision,
    ) -> Result<MoveDurability> {
        let _guard = self.lock_mutations()?;
        Self::require_trash_payload(payload, TrashArea::Items)?;
        Self::require_content_path(destination)?;
        let source = payload.as_vault_path();
        self.verify_expected_inner(source, expected, MAX_TRASH_PAYLOAD_BYTES)?;
        let durability = match self.atomic_move_locked(source, destination, |_, directory| {
            sync_directory_for_move(directory)
        }) {
            Ok(durability) => durability,
            Err(CoreError::AtomicMoveOutcomeUnknown { .. }) => self.confirm_move_durability(
                source,
                destination,
                expected,
                MAX_TRASH_PAYLOAD_BYTES,
            )?,
            Err(error) => return Err(error),
        };
        self.verify_published_topology(source, destination, expected, MAX_TRASH_PAYLOAD_BYTES)
            .map_err(|verification| {
                Self::verified_move_unknown_after_success(
                    source,
                    destination,
                    durability,
                    verification,
                )
            })?;
        Ok(durability)
    }

    fn atomic_move_inner<F>(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        intent: WriteIntent,
        sync: F,
    ) -> Result<MoveDurability>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
    {
        let _guard = self.lock_mutations()?;
        Self::validate_mutation_policy(source, intent)?;
        Self::validate_mutation_policy(destination, intent)?;
        self.atomic_move_locked(source, destination, sync)
    }

    fn atomic_move_locked<F>(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        mut sync: F,
    ) -> Result<MoveDurability>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
    {
        if source == destination {
            return Err(CoreError::AlreadyExists(destination.as_path().to_owned()));
        }

        let (source_parent, source_name) = self.open_parent_checked(source)?;
        self.reject_final_symlink(&source_parent, &source_name, source)?;
        let source_metadata = source_parent.symlink_metadata(&source_name)?;
        if source_metadata.file_type().is_symlink() {
            return Err(CoreError::SymlinkRejected(
                self.root_path.join(source.as_path()),
            ));
        }
        if !source_metadata.is_file() && !source_metadata.is_dir() {
            return Err(CoreError::InvalidMove {
                source_path: source.as_path().to_owned(),
                destination_path: destination.as_path().to_owned(),
                reason: "source must be a regular file or directory",
            });
        }
        if source_metadata.is_dir()
            && destination
                .as_str()
                .strip_prefix(source.as_str())
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            return Err(CoreError::InvalidMove {
                source_path: source.as_path().to_owned(),
                destination_path: destination.as_path().to_owned(),
                reason: "a directory cannot be moved into its own descendant",
            });
        }

        let (destination_parent, destination_name) = self.open_parent_checked(destination)?;
        let destination_utf8 = destination_name
            .to_str()
            .ok_or_else(|| CoreError::InvalidRelativePath(destination.as_path().to_owned()))?;
        self.reject_sibling_collision(&destination_parent, destination_utf8, destination)?;

        let source_parent_path = source
            .as_str()
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent);
        let destination_parent_path = destination
            .as_str()
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent);

        if let Err(error) = rename_noreplace(
            &source_parent,
            &source_name,
            &destination_parent,
            &destination_name,
        ) {
            return Err(Self::map_atomic_move_error(source, destination, error));
        }

        let destination_sync = sync(MoveSyncParent::Destination, &destination_parent);
        let source_sync = if source_parent_path == destination_parent_path {
            None
        } else {
            Some(sync(MoveSyncParent::Source, &source_parent))
        };
        Self::finish_atomic_move_sync(source, destination, destination_sync, source_sync)
    }

    fn revision_inner(&self, relative: &VaultPath, max_bytes: usize) -> Result<FileRevision> {
        let (parent, name) = self.open_parent_checked(relative)?;
        self.revision_from_parent(&parent, &name, relative, max_bytes)
    }

    fn revision_from_parent(
        &self,
        parent: &Dir,
        name: &OsString,
        relative: &VaultPath,
        max_bytes: usize,
    ) -> Result<FileRevision> {
        self.reject_final_symlink(parent, name, relative)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = parent.open_with(name, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(CoreError::RevisionTargetNotFile(
                relative.as_path().to_owned(),
            ));
        }
        let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
        if metadata.len() > max_bytes_u64 {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "revision bytes",
                limit: max_bytes,
            });
        }

        let mut hasher = blake3::Hasher::new();
        let mut total = 0_usize;
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        loop {
            let remaining_with_probe = max_bytes.saturating_sub(total).saturating_add(1);
            let read_limit = buffer.len().min(remaining_with_probe);
            let count = file.read(&mut buffer[..read_limit])?;
            if count == 0 {
                break;
            }
            total = total.saturating_add(count);
            if total > max_bytes {
                return Err(CoreError::ResourceLimitExceeded {
                    resource: "revision bytes",
                    limit: max_bytes,
                });
            }
            hasher.update(&buffer[..count]);
        }
        Ok(FileRevision {
            hex: hasher.finalize().to_hex().to_string(),
            byte_len: u64::try_from(total).unwrap_or(u64::MAX),
        })
    }

    fn verify_expected_inner(
        &self,
        relative: &VaultPath,
        expected: &FileRevision,
        max_bytes: usize,
    ) -> Result<()> {
        expected.validate()?;
        let actual = self.revision_inner(relative, max_bytes)?;
        if &actual == expected {
            Ok(())
        } else {
            Err(CoreError::StaleRevision {
                path: relative.as_path().to_owned(),
                expected: expected.clone(),
                actual,
            })
        }
    }

    /// Confirms and re-syncs an outcome-unknown verified move. The caller must
    /// hold the shared root mutation lock and must have already validated the
    /// privileged endpoint roles.
    fn confirm_move_durability(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        expected: &FileRevision,
        max_bytes: usize,
    ) -> Result<MoveDurability> {
        self.confirm_move_durability_with_sync(
            source,
            destination,
            expected,
            max_bytes,
            |_, directory| sync_directory_for_move(directory),
        )
    }

    fn confirm_move_durability_with_sync<F>(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        expected: &FileRevision,
        max_bytes: usize,
        mut sync: F,
    ) -> Result<MoveDurability>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
    {
        let (destination_parent, destination_name) = self
            .open_parent_checked(destination)
            .map_err(|verification| {
                Self::verified_move_unknown(
                    source,
                    destination,
                    DirectorySyncStatus::NotAttempted,
                    DirectorySyncStatus::NotAttempted,
                    verification,
                )
            })?;
        let (source_parent, source_name) =
            self.open_parent_checked(source).map_err(|verification| {
                Self::verified_move_unknown(
                    source,
                    destination,
                    DirectorySyncStatus::NotAttempted,
                    DirectorySyncStatus::NotAttempted,
                    verification,
                )
            })?;
        let same_parent = source
            .as_str()
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            == destination
                .as_str()
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent);
        // Durability closure comes first. Both attempts are made before any
        // revision/topology observation can fail or be influenced by an
        // external writer.
        let destination_sync = sync(MoveSyncParent::Destination, &destination_parent);
        let source_sync = (!same_parent).then(|| sync(MoveSyncParent::Source, &source_parent));

        let verification = self
            .verify_expected_from_parent(
                &destination_parent,
                &destination_name,
                destination,
                expected,
                max_bytes,
            )
            .and_then(|()| {
                Self::verify_source_absent(&source_parent, &source_name, source, destination)
            });
        if let Err(verification) = verification {
            return Err(Self::verified_move_unknown(
                source,
                destination,
                directory_sync_status(destination_sync),
                source_sync.map_or(
                    DirectorySyncStatus::SharedWithDestination,
                    directory_sync_status,
                ),
                verification,
            ));
        }
        Self::finish_atomic_move_sync(source, destination, destination_sync, source_sync)
    }

    fn verify_expected_from_parent(
        &self,
        parent: &Dir,
        name: &OsString,
        relative: &VaultPath,
        expected: &FileRevision,
        max_bytes: usize,
    ) -> Result<()> {
        expected.validate()?;
        let actual = self.revision_from_parent(parent, name, relative, max_bytes)?;
        if &actual == expected {
            Ok(())
        } else {
            Err(CoreError::StaleRevision {
                path: relative.as_path().to_owned(),
                expected: expected.clone(),
                actual,
            })
        }
    }

    fn verify_published_topology(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        expected: &FileRevision,
        max_bytes: usize,
    ) -> Result<()> {
        self.verify_expected_inner(destination, expected, max_bytes)?;
        let (source_parent, source_name) = self.open_parent_checked(source)?;
        Self::verify_source_absent(&source_parent, &source_name, source, destination)
    }

    fn verify_source_absent(
        source_parent: &Dir,
        source_name: &OsString,
        source: &VaultPath,
        destination: &VaultPath,
    ) -> Result<()> {
        match source_parent.symlink_metadata(source_name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
            Ok(_) => Err(CoreError::InvalidMove {
                source_path: source.as_path().to_owned(),
                destination_path: destination.as_path().to_owned(),
                reason: "cannot confirm move durability while the source still exists",
            }),
        }
    }

    fn verified_move_unknown_after_success(
        source: &VaultPath,
        destination: &VaultPath,
        durability: MoveDurability,
        verification: CoreError,
    ) -> CoreError {
        let destination_sync = match durability {
            MoveDurability::FullySynced => DirectorySyncStatus::Synced,
            MoveDurability::DirectorySyncUnsupported => DirectorySyncStatus::Unsupported,
        };
        let same_parent = source
            .as_str()
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            == destination
                .as_str()
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent);
        let source_sync = if same_parent {
            DirectorySyncStatus::SharedWithDestination
        } else {
            match durability {
                MoveDurability::FullySynced => DirectorySyncStatus::Synced,
                MoveDurability::DirectorySyncUnsupported => DirectorySyncStatus::Unsupported,
            }
        };
        Self::verified_move_unknown(
            source,
            destination,
            destination_sync,
            source_sync,
            verification,
        )
    }

    fn verified_move_unknown(
        source: &VaultPath,
        destination: &VaultPath,
        destination_sync: DirectorySyncStatus,
        source_sync: DirectorySyncStatus,
        verification: CoreError,
    ) -> CoreError {
        CoreError::VerifiedMoveOutcomeUnknown {
            source_path: source.as_path().to_owned(),
            destination_path: destination.as_path().to_owned(),
            destination_sync,
            source_sync,
            verification: Box::new(verification),
        }
    }

    fn atomic_write_inner<F>(
        &self,
        relative: &VaultPath,
        contents: &[u8],
        intent: WriteIntent,
        after_parent_open: F,
    ) -> Result<()>
    where
        F: FnOnce(),
    {
        Self::validate_mutation_policy(relative, intent)?;

        let (parent, destination_name) = self.open_parent_checked(relative)?;
        let destination_utf8 = destination_name
            .to_str()
            .ok_or_else(|| CoreError::InvalidRelativePath(relative.as_path().to_owned()))?;
        self.reject_sibling_collision(&parent, destination_utf8, relative)?;
        self.reject_final_symlink(&parent, &destination_name, relative)?;
        after_parent_open();

        let (temp_name, mut file) = Self::create_temp(&parent)?;
        let result = (|| {
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);

            parent.rename(&temp_name, &parent, &destination_name)?;
            sync_directory(&parent)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = parent.remove_file(&temp_name);
        }
        result
    }

    fn open_parent(&self, relative: &VaultPath) -> Result<(Dir, OsString)> {
        let components: Vec<_> = relative.as_path().components().collect();
        let (name, parents) = components
            .split_last()
            .ok_or_else(|| CoreError::InvalidRelativePath(relative.as_path().to_path_buf()))?;
        let mut current = self.root_dir.try_clone()?;
        let mut display = self.root_path.clone();
        for component in parents {
            display.push(component.as_os_str());
            current = open_child_dir_nofollow(&current, component.as_os_str(), &display)?;
        }
        Ok((current, name.as_os_str().to_owned()))
    }

    fn open_parent_checked(&self, relative: &VaultPath) -> Result<(Dir, OsString)> {
        let components: Vec<_> = relative.as_path().components().collect();
        let (name, parents) = components
            .split_last()
            .ok_or_else(|| CoreError::InvalidRelativePath(relative.as_path().to_path_buf()))?;
        let mut current = self.root_dir.try_clone()?;
        let mut display = self.root_path.clone();
        for component in parents {
            let component_name = component.as_os_str();
            let component_utf8 = component_name
                .to_str()
                .ok_or_else(|| CoreError::InvalidRelativePath(relative.as_path().to_path_buf()))?;
            self.reject_sibling_collision(&current, component_utf8, relative)?;
            display.push(component_name);
            current = open_child_dir_nofollow(&current, component_name, &display)?;
        }
        Ok((current, name.as_os_str().to_owned()))
    }

    fn inventory_dir(
        &self,
        directory: &Dir,
        prefix: &[String],
        depth: usize,
        limits: InventoryLimits,
        visited: &mut usize,
        output: &mut Vec<InventoryEntry>,
    ) -> Result<()> {
        let remaining = limits.max_entries.saturating_sub(*visited);
        let mut entries = Vec::with_capacity(remaining.min(1024).saturating_add(1));
        for entry in directory.read_dir(".")? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name_utf8) = name.to_str() else {
                return Err(CoreError::InvalidRelativePath(self.root_path.join(&name)));
            };
            if prefix.is_empty() && classify_component(name_utf8) != VaultPathClass::Content {
                continue;
            }
            if entries.len() > remaining {
                return Err(CoreError::ResourceLimitExceeded {
                    resource: "inventory entries",
                    limit: limits.max_entries,
                });
            }
            entries.push(entry);
        }
        if entries.len() > remaining {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "inventory entries",
                limit: limits.max_entries,
            });
        }
        entries.sort_unstable_by_key(cap_std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let Some(name_utf8) = name.to_str() else {
                return Err(CoreError::InvalidRelativePath(self.root_path.join(&name)));
            };
            *visited = visited.saturating_add(1);
            if *visited > limits.max_entries {
                return Err(CoreError::ResourceLimitExceeded {
                    resource: "inventory entries",
                    limit: limits.max_entries,
                });
            }
            let mut components = prefix.to_owned();
            components.push(name_utf8.to_owned());
            let path = VaultPath::from_portable(components.join("/"))?;
            let metadata = directory.symlink_metadata(&name)?;
            if metadata.file_type().is_symlink() {
                return Err(CoreError::SymlinkRejected(
                    self.root_path.join(path.as_path()),
                ));
            }
            if metadata.is_dir() {
                let next_depth = depth.saturating_add(1);
                if next_depth > limits.max_depth {
                    return Err(CoreError::ResourceLimitExceeded {
                        resource: "inventory depth",
                        limit: limits.max_depth,
                    });
                }
                let display = self.root_path.join(path.as_path());
                let child = open_child_dir_nofollow(directory, &name, &display)?;
                self.inventory_dir(&child, &components, next_depth, limits, visited, output)?;
            } else if metadata.is_file() {
                let markdown = Path::new(name_utf8)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("md")
                            || extension.eq_ignore_ascii_case("markdown")
                    });
                output.push(InventoryEntry {
                    path,
                    kind: if markdown {
                        InventoryKind::Markdown
                    } else {
                        InventoryKind::File
                    },
                    size: metadata.len(),
                });
            }
        }
        Ok(())
    }

    fn reject_final_symlink(
        &self,
        parent: &Dir,
        name: &OsString,
        relative: &VaultPath,
    ) -> Result<()> {
        if parent
            .symlink_metadata(name)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(CoreError::SymlinkRejected(
                self.root_path.join(relative.as_path()),
            ));
        }
        Ok(())
    }

    fn lock_mutations(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.mutation_lock
            .lock()
            .map_err(|_| std::io::Error::other("vault mutation lock was poisoned").into())
    }

    fn reject_sibling_collision(
        &self,
        parent: &Dir,
        desired: &str,
        incoming: &VaultPath,
    ) -> Result<()> {
        let desired_key = component_collision_key(desired);
        for entry in parent.read_dir(".")? {
            let existing = entry?.file_name();
            let Some(existing_utf8) = existing.to_str() else {
                return Err(CoreError::InvalidRelativePath(
                    self.root_path.join(existing),
                ));
            };
            if existing_utf8 != desired && component_collision_key(existing_utf8) == desired_key {
                return Err(CoreError::PortablePathCollision {
                    existing: existing_utf8.to_owned(),
                    incoming: incoming.as_str().to_owned(),
                });
            }
        }
        Ok(())
    }

    fn validate_mutation_policy(relative: &VaultPath, intent: WriteIntent) -> Result<()> {
        match relative.classify() {
            VaultPathClass::ObsidianMetadata if intent == WriteIntent::Automatic => Err(
                CoreError::AutomaticObsidianWriteDenied(relative.as_path().to_owned()),
            ),
            VaultPathClass::Trash => {
                Err(CoreError::TrashWriteDenied(relative.as_path().to_owned()))
            }
            VaultPathClass::Content | VaultPathClass::ObsidianMetadata => Ok(()),
        }
    }

    fn validate_generic_access(relative: &VaultPath) -> Result<()> {
        if relative.classify() == VaultPathClass::Trash {
            Err(CoreError::TrashAccessDenied(relative.as_path().to_owned()))
        } else {
            Ok(())
        }
    }

    fn require_content_path(relative: &VaultPath) -> Result<()> {
        if relative.classify() == VaultPathClass::Content {
            Ok(())
        } else {
            Err(CoreError::InvalidMove {
                source_path: relative.as_path().to_owned(),
                destination_path: relative.as_path().to_owned(),
                reason: "privileged trash moves require a content path",
            })
        }
    }

    fn require_trash_payload(path: &TrashPath, area: TrashArea) -> Result<()> {
        if path.area() == area && path.kind() == TrashEntryKind::Payload {
            Ok(())
        } else {
            Err(CoreError::InvalidTrashPath(
                path.as_vault_path().as_path().to_owned(),
            ))
        }
    }

    fn commit_unknown(path: PathBuf, error: CoreError) -> CoreError {
        match error {
            CoreError::Io(source) => CoreError::CommitOutcomeUnknown { path, source },
            other => other,
        }
    }

    fn map_atomic_move_error(
        source: &VaultPath,
        destination: &VaultPath,
        error: std::io::Error,
    ) -> CoreError {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return CoreError::AlreadyExists(destination.as_path().to_owned());
        }
        if atomic_no_replace_is_unsupported(&error) {
            return CoreError::AtomicNoReplaceUnsupported {
                source_path: source.as_path().to_owned(),
                destination_path: destination.as_path().to_owned(),
                source: error,
            };
        }
        CoreError::Io(error)
    }

    fn finish_atomic_move_sync(
        source: &VaultPath,
        destination: &VaultPath,
        destination_sync: std::io::Result<MoveDurability>,
        source_sync: Option<std::io::Result<MoveDurability>>,
    ) -> Result<MoveDurability> {
        match (destination_sync, source_sync) {
            (Ok(destination), None) => Ok(destination),
            (Ok(destination), Some(Ok(source))) => Ok(destination.combine(source)),
            (destination_result, source_result) => Err(CoreError::AtomicMoveOutcomeUnknown {
                source_path: source.as_path().to_owned(),
                destination_path: destination.as_path().to_owned(),
                destination_sync: directory_sync_status(destination_result),
                source_sync: source_result.map_or(
                    DirectorySyncStatus::SharedWithDestination,
                    directory_sync_status,
                ),
            }),
        }
    }

    fn cleanup_pending(path: PathBuf, temp_name: &OsString, source: std::io::Error) -> CoreError {
        CoreError::PublishedCleanupPending {
            path,
            temp_name: temp_name.to_string_lossy().into_owned(),
            source,
        }
    }

    fn cleanup_pending_from_core(
        path: PathBuf,
        temp_name: &OsString,
        error: CoreError,
    ) -> CoreError {
        match error {
            CoreError::Io(source) => Self::cleanup_pending(path, temp_name, source),
            other => other,
        }
    }

    fn create_temp(parent: &Dir) -> Result<(OsString, cap_std::fs::File)> {
        for _ in 0..64 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let candidate =
                OsString::from(format!(".myvault-write-{}-{id}.tmp", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            options.follow(FollowSymlinks::No);
            match parent.open_with(&candidate, &options) {
                Ok(file) => return Ok((candidate, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "unable to allocate atomic-write temporary name",
        )
        .into())
    }
}

fn shared_mutation_lock(root: &Path) -> Result<Arc<Mutex<()>>> {
    let registry = MUTATION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = registry
        .lock()
        .map_err(|_| std::io::Error::other("vault mutation-lock registry was poisoned"))?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(root.to_owned(), Arc::downgrade(&lock));
    Ok(lock)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CreateStage {
    LinkPublished,
    DirectorySynced,
    TempRemoved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MoveSyncParent {
    Destination,
    Source,
}

fn sync_directory(directory: &Dir) -> Result<()> {
    let file = directory.try_clone()?.into_std_file();
    match file.sync_all() {
        Ok(()) => Ok(()),
        // Some Windows/filesystem combinations do not support flushing a
        // directory handle. The file itself was flushed before atomic rename.
        #[cfg(windows)]
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

impl MoveDurability {
    fn combine(self, other: Self) -> Self {
        if self == Self::DirectorySyncUnsupported || other == Self::DirectorySyncUnsupported {
            Self::DirectorySyncUnsupported
        } else {
            Self::FullySynced
        }
    }
}

fn sync_directory_for_move(directory: &Dir) -> std::io::Result<MoveDurability> {
    let file = directory.try_clone()?.into_std_file();
    match file.sync_all() {
        Ok(()) => Ok(MoveDurability::FullySynced),
        #[cfg(windows)]
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::Unsupported
            ) =>
        {
            Ok(MoveDurability::DirectorySyncUnsupported)
        }
        Err(error) => Err(error),
    }
}

fn directory_sync_status(result: std::io::Result<MoveDurability>) -> DirectorySyncStatus {
    match result {
        Ok(MoveDurability::FullySynced) => DirectorySyncStatus::Synced,
        Ok(MoveDurability::DirectorySyncUnsupported) => DirectorySyncStatus::Unsupported,
        Err(error) => DirectorySyncStatus::Failed(error),
    }
}

fn atomic_no_replace_is_unsupported(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::Unsupported {
        return true;
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
    {
        let code = error.raw_os_error();
        [
            rustix::io::Errno::NOSYS,
            rustix::io::Errno::NOTSUP,
            rustix::io::Errno::OPNOTSUPP,
        ]
        .into_iter()
        .any(|errno| code == Some(errno.raw_os_error()))
    }

    #[cfg(not(any(target_os = "android", target_os = "linux", target_os = "macos")))]
    false
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn parent_symlink_swap_cannot_redirect_atomic_commit() {
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let base = temp_root.join(format!(
            "myvault-adversarial-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let vault_path = base.join("vault");
        let outside = base.join("outside");
        fs::create_dir_all(vault_path.join("notes")).expect("vault fixture");
        fs::create_dir_all(&outside).expect("outside fixture");
        let vault = Vault::open(&vault_path).expect("open vault");
        let relative = VaultPath::new("notes/attack.md").expect("path");

        vault
            .atomic_write_inner(&relative, b"safe", WriteIntent::Automatic, || {
                fs::rename(vault_path.join("notes"), vault_path.join("original-notes"))
                    .expect("swap original parent");
                symlink(&outside, vault_path.join("notes")).expect("install attack symlink");
            })
            .expect("descriptor-relative commit");

        assert_eq!(
            fs::read(vault_path.join("original-notes/attack.md")).expect("safe destination"),
            b"safe"
        );
        assert!(!outside.join("attack.md").exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    fn create_outcome_fixture(label: &str) -> (PathBuf, Vault, VaultPath) {
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let base = temp_root.join(format!(
            "myvault-create-outcome-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).expect("vault fixture");
        let vault = Vault::open(&base).expect("open vault");
        (base, vault, VaultPath::new("note.md").expect("path"))
    }

    fn injected_error() -> std::io::Error {
        std::io::Error::other("injected create stage failure")
    }

    fn temp_files(base: &Path) -> Vec<PathBuf> {
        fs::read_dir(base)
            .expect("read fixture")
            .filter_map(|entry| {
                let path = entry.expect("entry").path();
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".myvault-write-"))
                    .then_some(path)
            })
            .collect()
    }

    #[test]
    fn atomic_move_maps_an_unsupported_primitive_without_fallback() {
        let source = VaultPath::new("source.md").expect("source path");
        let destination = VaultPath::new("destination.md").expect("destination path");
        let error = Vault::map_atomic_move_error(
            &source,
            &destination,
            std::io::Error::new(std::io::ErrorKind::Unsupported, "injected no-replace fault"),
        );

        assert!(matches!(
            error,
            CoreError::AtomicNoReplaceUnsupported {
                source_path,
                destination_path,
                ..
            } if source_path == source.as_path() && destination_path == destination.as_path()
        ));
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
    #[test]
    fn atomic_move_does_not_misclassify_generic_einval_as_unsupported() {
        let source = VaultPath::new("source.md").expect("source path");
        let destination = VaultPath::new("destination.md").expect("destination path");
        let error = Vault::map_atomic_move_error(
            &source,
            &destination,
            std::io::Error::from_raw_os_error(rustix::io::Errno::INVAL.raw_os_error()),
        );

        assert!(matches!(error, CoreError::Io(_)));
    }

    fn atomic_move_sync_fixture(label: &str) -> (PathBuf, Vault, VaultPath, VaultPath) {
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let base = temp_root.join(format!(
            "myvault-move-sync-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(base.join("source-parent")).expect("source parent");
        fs::create_dir(base.join("destination-parent")).expect("destination parent");
        fs::write(base.join("source-parent/note.md"), b"note").expect("source");
        let vault = Vault::open(&base).expect("open vault");
        (
            base,
            vault,
            VaultPath::new("source-parent/note.md").expect("source path"),
            VaultPath::new("destination-parent/note.md").expect("destination path"),
        )
    }

    fn injected_sync_failure(parent: MoveSyncParent) -> std::io::Error {
        std::io::Error::other(format!("injected {parent:?} sync failure"))
    }

    #[test]
    fn atomic_move_attempts_source_sync_after_destination_sync_fails() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("destination-fails");
        let mut attempts = Vec::new();
        let error = vault
            .atomic_move_inner(
                &source,
                &destination,
                WriteIntent::UserInitiated,
                |parent, _| {
                    attempts.push(parent);
                    if parent == MoveSyncParent::Destination {
                        Err(injected_sync_failure(parent))
                    } else {
                        Ok(MoveDurability::FullySynced)
                    }
                },
            )
            .expect_err("destination sync failure");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert!(matches!(
            error,
            CoreError::AtomicMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Failed(_),
                source_sync: DirectorySyncStatus::Synced,
                ..
            }
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn atomic_move_reports_source_sync_failure_after_destination_passes() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("source-fails");
        let error = vault
            .atomic_move_inner(
                &source,
                &destination,
                WriteIntent::UserInitiated,
                |parent, _| {
                    if parent == MoveSyncParent::Source {
                        Err(injected_sync_failure(parent))
                    } else {
                        Ok(MoveDurability::FullySynced)
                    }
                },
            )
            .expect_err("source sync failure");

        assert!(matches!(
            error,
            CoreError::AtomicMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Failed(_),
                ..
            }
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn atomic_move_reports_both_parent_sync_failures() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("both-fail");
        let mut attempts = Vec::new();
        let error = vault
            .atomic_move_inner(
                &source,
                &destination,
                WriteIntent::UserInitiated,
                |parent, _| {
                    attempts.push(parent);
                    Err(injected_sync_failure(parent))
                },
            )
            .expect_err("both sync failures");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert!(matches!(
            error,
            CoreError::AtomicMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Failed(_),
                source_sync: DirectorySyncStatus::Failed(_),
                ..
            }
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn confirm_move_durability_requires_expected_destination_and_absent_source() {
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let base = temp_root.join(format!(
            "myvault-confirm-move-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(base.join("from")).expect("source parent");
        fs::create_dir(base.join("to")).expect("destination parent");
        fs::write(base.join("to/note.md"), b"note").expect("destination");
        let vault = Vault::open(&base).expect("open vault");
        let source = VaultPath::new("from/note.md").expect("source");
        let destination = VaultPath::new("to/note.md").expect("destination");
        let expected = FileRevision::from_bytes(b"note");
        let guard = vault.lock_mutations().expect("lock");

        vault
            .confirm_move_durability(&source, &destination, &expected, 4)
            .expect("confirmed durability");
        fs::write(base.join("from/note.md"), b"duplicate").expect("duplicate source");
        assert!(matches!(
            vault.confirm_move_durability(&source, &destination, &expected, 4),
            Err(CoreError::VerifiedMoveOutcomeUnknown {
                verification,
                ..
            }) if matches!(*verification, CoreError::InvalidMove { .. })
        ));
        drop(guard);
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn confirm_resyncs_both_parents_before_reporting_mutated_destination() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("confirm-destination");
        let guard = vault.lock_mutations().expect("lock");
        let initial = vault.atomic_move_locked(&source, &destination, |parent, _| {
            if parent == MoveSyncParent::Destination {
                Err(injected_sync_failure(parent))
            } else {
                Ok(MoveDurability::FullySynced)
            }
        });
        assert!(matches!(
            initial,
            Err(CoreError::AtomicMoveOutcomeUnknown { .. })
        ));
        fs::write(base.join(destination.as_path()), b"external").expect("mutate destination");

        let mut attempts = Vec::new();
        let error = vault
            .confirm_move_durability_with_sync(
                &source,
                &destination,
                &FileRevision::from_bytes(b"note"),
                64,
                |parent, _| {
                    attempts.push(parent);
                    Ok(MoveDurability::FullySynced)
                },
            )
            .expect_err("mutated destination is outcome unknown");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::StaleRevision { .. })
        ));
        drop(guard);
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn confirm_resyncs_both_parents_before_reporting_recreated_source() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("confirm-source");
        let guard = vault.lock_mutations().expect("lock");
        let initial = vault.atomic_move_locked(&source, &destination, |parent, _| {
            if parent == MoveSyncParent::Source {
                Err(injected_sync_failure(parent))
            } else {
                Ok(MoveDurability::FullySynced)
            }
        });
        assert!(matches!(
            initial,
            Err(CoreError::AtomicMoveOutcomeUnknown { .. })
        ));
        fs::write(base.join(source.as_path()), b"external duplicate").expect("recreate source");

        let mut attempts = Vec::new();
        let error = vault
            .confirm_move_durability_with_sync(
                &source,
                &destination,
                &FileRevision::from_bytes(b"note"),
                64,
                |parent, _| {
                    attempts.push(parent);
                    Ok(MoveDurability::FullySynced)
                },
            )
            .expect_err("recreated source is outcome unknown");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidMove { .. })
        ));
        drop(guard);
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn create_reports_unknown_outcome_after_link_and_retains_temp() {
        let (base, vault, path) = create_outcome_fixture("after-link");
        let error = vault
            .create_new_inner(&path, b"published", WriteIntent::UserInitiated, |stage| {
                (stage != CreateStage::LinkPublished)
                    .then_some(())
                    .ok_or_else(injected_error)
            })
            .expect_err("injected failure");
        assert!(matches!(error, CoreError::CommitOutcomeUnknown { .. }));
        assert_eq!(
            fs::read(base.join("note.md")).expect("destination"),
            b"published"
        );
        assert_eq!(temp_files(&base).len(), 1);
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn create_reports_cleanup_pending_after_first_sync_and_retains_temp() {
        let (base, vault, path) = create_outcome_fixture("after-sync");
        let error = vault
            .create_new_inner(&path, b"published", WriteIntent::UserInitiated, |stage| {
                (stage != CreateStage::DirectorySynced)
                    .then_some(())
                    .ok_or_else(injected_error)
            })
            .expect_err("injected failure");
        assert!(matches!(error, CoreError::PublishedCleanupPending { .. }));
        assert_eq!(
            fs::read(base.join("note.md")).expect("destination"),
            b"published"
        );
        assert_eq!(temp_files(&base).len(), 1);
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn create_reports_cleanup_durability_after_temp_removal() {
        let (base, vault, path) = create_outcome_fixture("after-cleanup");
        let error = vault
            .create_new_inner(&path, b"published", WriteIntent::UserInitiated, |stage| {
                (stage != CreateStage::TempRemoved)
                    .then_some(())
                    .ok_or_else(injected_error)
            })
            .expect_err("injected failure");
        assert!(matches!(error, CoreError::PublishedCleanupPending { .. }));
        assert_eq!(
            fs::read(base.join("note.md")).expect("destination"),
            b"published"
        );
        assert!(temp_files(&base).is_empty());
        fs::remove_dir_all(base).expect("cleanup");
    }
}
