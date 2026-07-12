use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsExt;
use cap_fs_ext::{FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};

use crate::atomic_move::rename_noreplace;
use crate::capability::{open_absolute_dir_nofollow, open_child_dir_nofollow};
use crate::path::{classify_component, component_collision_key, VaultPathClass};
use crate::trash::{item_directory_path, manifest_path, payload_path};
use crate::{
    CoreError, FileRevision, ManifestDigest, PrepareManifestOutcome, PublishItemOutcome,
    RestoreItemOutcome, Result, StagePayloadOutcome, TrashArea, TrashId, TrashManifestV1,
    TrashStore, VaultPath, MAX_TRASH_MANIFEST_BYTES,
};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveContentOutcome {
    Moved(MoveDurability),
    AlreadyMoved(MoveDurability),
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

    #[must_use]
    pub fn trash_store(&self) -> TrashStore<'_> {
        TrashStore::new(self)
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

    /// Atomically moves one revision-bound content file without replacing an
    /// existing destination, and safely confirms exact retries.
    ///
    /// # Errors
    /// Returns a typed prepublication sync error when the file is known not to
    /// have moved, or an explicit outcome-unknown error after publication.
    pub fn move_content_file_if_revision(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        expected: &FileRevision,
    ) -> Result<MoveContentOutcome> {
        self.move_content_file_with_hooks(
            source,
            destination,
            expected,
            |_, directory| sync_directory_for_move(directory),
            || {},
            || {},
        )
    }

    #[allow(clippy::too_many_lines)]
    fn move_content_file_with_hooks<F, G, H>(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        expected: &FileRevision,
        mut sync: F,
        before_rename: G,
        after_move: H,
    ) -> Result<MoveContentOutcome>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
        G: FnOnce(),
        H: FnOnce(),
    {
        Self::require_content_path(source)?;
        Self::require_content_path(destination)?;
        if source == destination || source.collision_key() == destination.collision_key() {
            return Err(CoreError::InvalidMove {
                source_path: source.as_path().to_owned(),
                destination_path: destination.as_path().to_owned(),
                reason: "content move endpoints must have distinct portable collision keys",
            });
        }
        expected.validate()?;
        if expected.byte_len > MAX_TRASH_PAYLOAD_BYTES as u64 {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "move content revision bytes",
                limit: MAX_TRASH_PAYLOAD_BYTES,
            });
        }
        let _guard = self.lock_mutations()?;
        let (destination_parent, _) = self.open_parent(destination)?;
        let (source_parent, _) = self.open_parent(source)?;
        let same_parent = Self::same_parent_path(source, destination);
        let classification_report = MoveSyncReport {
            destination: sync(MoveSyncParent::Destination, &destination_parent),
            source: (!same_parent).then(|| sync(MoveSyncParent::Source, &source_parent)),
        };
        if classification_report.has_failure() {
            return Err(
                classification_report.into_content_prepublication_sync_failed(
                    source,
                    destination,
                    CoreError::MoveDurabilitySyncFailed,
                ),
            );
        }
        let classification_durability = classification_report.into_result(source, destination)?;
        let (authoritative_destination, checked_destination_name) =
            self.open_parent_checked(destination)?;
        let (authoritative_source, checked_source_name) = self.open_parent_checked(source)?;
        if !Self::same_open_directory(&destination_parent, &authoritative_destination)?
            || !Self::same_open_directory(&source_parent, &authoritative_source)?
        {
            return Err(CoreError::InvalidMove {
                source_path: source.as_path().to_owned(),
                destination_path: destination.as_path().to_owned(),
                reason: "content parent directory identity changed",
            });
        }
        let source_utf8 = checked_source_name
            .to_str()
            .ok_or_else(|| CoreError::InvalidRelativePath(source.as_path().to_owned()))?;
        let destination_utf8 = checked_destination_name
            .to_str()
            .ok_or_else(|| CoreError::InvalidRelativePath(destination.as_path().to_owned()))?;
        self.reject_sibling_collision(&authoritative_source, source_utf8, source)?;
        self.reject_sibling_collision(&authoritative_destination, destination_utf8, destination)?;
        let source_exists =
            Self::entry_exists_from_parent(&authoritative_source, &checked_source_name)?;
        let destination_exists =
            Self::entry_exists_from_parent(&authoritative_destination, &checked_destination_name)?;
        match (source_exists, destination_exists) {
            (true, true) => Err(CoreError::AlreadyExists(destination.as_path().to_owned())),
            (false, false) => Err(CoreError::InvalidMove {
                source_path: source.as_path().to_owned(),
                destination_path: destination.as_path().to_owned(),
                reason: "source and destination are both absent",
            }),
            (false, true) => {
                self.verify_content_move_topology(
                    source,
                    destination,
                    expected,
                    &authoritative_source,
                    &authoritative_destination,
                )?;
                Ok(MoveContentOutcome::AlreadyMoved(classification_durability))
            }
            (true, false) => {
                self.verify_expected_from_parent(
                    &authoritative_source,
                    &checked_source_name,
                    source,
                    expected,
                    MAX_TRASH_PAYLOAD_BYTES,
                )?;
                self.verify_single_link_from_parent(
                    &authoritative_source,
                    &checked_source_name,
                    source,
                )?;
                before_rename();
                let (latest_destination, latest_destination_name) =
                    self.open_parent_checked(destination)?;
                let (latest_source, latest_source_name) = self.open_parent_checked(source)?;
                if !Self::same_open_directory(&authoritative_destination, &latest_destination)?
                    || !Self::same_open_directory(&authoritative_source, &latest_source)?
                {
                    return Err(CoreError::InvalidMove {
                        source_path: source.as_path().to_owned(),
                        destination_path: destination.as_path().to_owned(),
                        reason: "content parent directory identity changed before rename",
                    });
                }
                rename_noreplace(
                    &latest_source,
                    &latest_source_name,
                    &latest_destination,
                    &latest_destination_name,
                )
                .map_err(|error| Self::map_atomic_move_error(source, destination, error))?;
                let move_report = MoveSyncReport {
                    destination: sync(MoveSyncParent::Destination, &latest_destination),
                    source: (!same_parent).then(|| sync(MoveSyncParent::Source, &latest_source)),
                };
                after_move();
                if move_report.has_failure() {
                    return self.confirm_content_move(
                        source,
                        destination,
                        expected,
                        &latest_source,
                        &latest_destination,
                        classification_durability,
                        &mut sync,
                    );
                }
                if let Err(cause) = self.verify_content_move_topology(
                    source,
                    destination,
                    expected,
                    &latest_source,
                    &latest_destination,
                ) {
                    return Err(move_report.into_verified_unknown(source, destination, cause));
                }
                let durability = classification_durability
                    .combine(move_report.into_result(source, destination)?);
                Ok(MoveContentOutcome::Moved(durability))
            }
        }
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
        self.atomic_move_locked_report(source, destination, sync)?
            .into_result(source, destination)
    }

    fn same_parent_path(source: &VaultPath, destination: &VaultPath) -> bool {
        source
            .as_str()
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            == destination
                .as_str()
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent)
    }

    fn entry_exists_from_parent(parent: &Dir, name: &OsString) -> Result<bool> {
        match parent.symlink_metadata(name) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn same_open_directory(expected: &Dir, actual: &Dir) -> Result<bool> {
        Ok(myvault_platform_fs::directory_identity(expected)?
            == myvault_platform_fs::directory_identity(actual)?)
    }

    fn verify_content_move_topology(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        expected: &FileRevision,
        expected_source_parent: &Dir,
        expected_destination_parent: &Dir,
    ) -> Result<()> {
        let (destination_parent, destination_name) = self.open_parent_checked(destination)?;
        let (source_parent, source_name) = self.open_parent_checked(source)?;
        if !Self::same_open_directory(expected_destination_parent, &destination_parent)?
            || !Self::same_open_directory(expected_source_parent, &source_parent)?
        {
            return Err(CoreError::InvalidMove {
                source_path: source.as_path().to_owned(),
                destination_path: destination.as_path().to_owned(),
                reason: "content parent directory identity changed",
            });
        }
        self.verify_expected_from_parent(
            &destination_parent,
            &destination_name,
            destination,
            expected,
            MAX_TRASH_PAYLOAD_BYTES,
        )?;
        self.verify_single_link_from_parent(&destination_parent, &destination_name, destination)?;
        Self::verify_source_absent(&source_parent, &source_name, source, destination)
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm_content_move<F>(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        expected: &FileRevision,
        expected_source_parent: &Dir,
        expected_destination_parent: &Dir,
        classification_durability: MoveDurability,
        sync: &mut F,
    ) -> Result<MoveContentOutcome>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
    {
        let same_parent = Self::same_parent_path(source, destination);
        let destination_open = self.open_parent(destination);
        let source_open = self.open_parent(source);
        let (destination_parent, source_parent) = match (destination_open, source_open) {
            (Ok((destination_parent, _)), Ok((source_parent, _))) => {
                (destination_parent, source_parent)
            }
            (Err(cause), Ok((source_parent, _))) => {
                let (destination_status, source_status) = if same_parent {
                    (
                        directory_sync_status(sync(MoveSyncParent::Destination, &source_parent)),
                        DirectorySyncStatus::SharedWithDestination,
                    )
                } else {
                    (
                        DirectorySyncStatus::NotAttempted,
                        directory_sync_status(sync(MoveSyncParent::Source, &source_parent)),
                    )
                };
                return Err(Self::verified_move_unknown(
                    source,
                    destination,
                    destination_status,
                    source_status,
                    cause,
                ));
            }
            (Ok((destination_parent, _)), Err(cause)) => {
                let destination_status =
                    directory_sync_status(sync(MoveSyncParent::Destination, &destination_parent));
                let source_status = if same_parent {
                    DirectorySyncStatus::SharedWithDestination
                } else {
                    DirectorySyncStatus::NotAttempted
                };
                return Err(Self::verified_move_unknown(
                    source,
                    destination,
                    destination_status,
                    source_status,
                    cause,
                ));
            }
            (Err(cause), Err(_)) => {
                return Err(Self::verified_move_unknown(
                    source,
                    destination,
                    DirectorySyncStatus::NotAttempted,
                    DirectorySyncStatus::NotAttempted,
                    cause,
                ));
            }
        };
        let report = MoveSyncReport {
            destination: sync(MoveSyncParent::Destination, &destination_parent),
            source: (!same_parent).then(|| sync(MoveSyncParent::Source, &source_parent)),
        };
        if let Err(cause) = self.verify_content_move_topology(
            source,
            destination,
            expected,
            expected_source_parent,
            expected_destination_parent,
        ) {
            return Err(report.into_verified_unknown(source, destination, cause));
        }
        if report.has_failure() {
            return Err(report.into_verified_unknown(
                source,
                destination,
                CoreError::MoveDurabilitySyncFailed,
            ));
        }
        let durability =
            classification_durability.combine(report.into_result(source, destination)?);
        Ok(MoveContentOutcome::Moved(durability))
    }

    fn atomic_move_locked_report<F>(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        mut sync: F,
    ) -> Result<MoveSyncReport>
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
        Ok(MoveSyncReport {
            destination: destination_sync,
            source: source_sync,
        })
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
    #[cfg(test)]
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

    #[cfg(test)]
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
        let report = MoveSyncReport {
            destination: sync(MoveSyncParent::Destination, &destination_parent),
            source: (!same_parent).then(|| sync(MoveSyncParent::Source, &source_parent)),
        };

        let verification = self
            .verify_expected_from_parent(
                &destination_parent,
                &destination_name,
                destination,
                expected,
                max_bytes,
            )
            .and_then(|()| {
                self.verify_single_link_from_parent(
                    &destination_parent,
                    &destination_name,
                    destination,
                )
            })
            .and_then(|()| {
                Self::verify_source_absent(&source_parent, &source_name, source, destination)
            });
        if let Err(verification) = verification {
            return Err(report.into_verified_unknown(source, destination, verification));
        }
        if report.has_failure() {
            return Err(report.into_verified_unknown(
                source,
                destination,
                CoreError::MoveDurabilitySyncFailed,
            ));
        }
        report.into_result(source, destination)
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

    #[cfg(test)]
    fn verify_published_topology(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        expected: &FileRevision,
        max_bytes: usize,
    ) -> Result<()> {
        self.verify_expected_inner(destination, expected, max_bytes)?;
        self.verify_single_link_regular_file(destination)?;
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

    #[cfg(test)]
    fn verify_single_link_regular_file(&self, relative: &VaultPath) -> Result<()> {
        let (parent, name) = self.open_parent_checked(relative)?;
        self.verify_single_link_from_parent(&parent, &name, relative)
    }

    fn verify_single_link_from_parent(
        &self,
        parent: &Dir,
        name: &OsString,
        relative: &VaultPath,
    ) -> Result<()> {
        self.reject_final_symlink(parent, name, relative)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = parent.open_with(name, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(CoreError::InvalidMove {
                source_path: relative.as_path().to_owned(),
                destination_path: relative.as_path().to_owned(),
                reason: "file must be regular with exactly one hard link",
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn finish_verified_move(
        &self,
        source: &VaultPath,
        destination: &VaultPath,
        expected: &FileRevision,
        max_bytes: usize,
        report: MoveSyncReport,
    ) -> Result<MoveDurability> {
        if let Err(verification) =
            self.verify_published_topology(source, destination, expected, max_bytes)
        {
            return Err(report.into_verified_unknown(source, destination, verification));
        }
        report.into_result(source, destination)
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

impl TrashStore<'_> {
    /// Creates one immutable canonical staging manifest, or confirms an exact
    /// existing manifest. No production path removes abandoned temp files.
    ///
    /// # Errors
    /// Returns an error for semantic/canonical mismatch, collisions, symlinks,
    /// nonregular files, unsafe hard links, or durability failures.
    pub fn prepare_staging_manifest(
        &self,
        id: TrashId,
        manifest: &TrashManifestV1,
    ) -> Result<PrepareManifestOutcome> {
        self.prepare_staging_manifest_with_sync(id, manifest, |_| Ok(()))
    }

    #[allow(clippy::too_many_lines)]
    fn prepare_staging_manifest_with_sync<F>(
        &self,
        id: TrashId,
        manifest: &TrashManifestV1,
        mut inject_sync: F,
    ) -> Result<PrepareManifestOutcome>
    where
        F: FnMut(ManifestSyncStage) -> std::io::Result<()>,
    {
        manifest.validate(Some(id))?;
        let bytes = manifest.canonical_bytes()?;
        let _guard = self.vault.lock_mutations()?;
        let (staging_parent, items_parent) = self.ensure_trash_containers()?;
        sync_directory(&items_parent)?;
        sync_directory(&staging_parent)?;
        let item_name = OsString::from(id.to_string());
        let staging_exists = Self::entry_exists(&staging_parent, &item_name)?;
        let items_exists = Self::entry_exists(&items_parent, &item_name)?;
        let manifest_relative = manifest_path(TrashArea::Staging, id)?;
        let directory = match (staging_exists, items_exists) {
            (true, true) => {
                return Err(CoreError::InvalidTrashTopology(
                    "staging and items directories both exist while preparing manifest",
                ));
            }
            (false, true) => {
                let items_manifest_relative = manifest_path(TrashArea::Items, id)?;
                let held_items_directory = self.open_trash_item_directory(
                    TrashArea::Items,
                    id,
                    &items_parent,
                    &item_name,
                )?;
                Self::sync_manifest_directory(
                    &held_items_directory,
                    ManifestSyncStage::ExistingDirectoryPrecheck,
                    &mut inject_sync,
                )
                .map_err(|cause| Self::manifest_outcome_unknown(&items_manifest_relative, cause))?;
                let items_directory = self.reopen_bound_trash_item_directory(
                    TrashArea::Items,
                    id,
                    &items_parent,
                    &item_name,
                    &held_items_directory,
                )?;
                if Self::entry_exists(&staging_parent, &item_name)? {
                    return Err(CoreError::InvalidTrashTopology(
                        "staging directory appeared while confirming published manifest",
                    ));
                }
                let stored = Self::read_manifest_from_directory(id, &items_directory, false)?;
                let digest = ManifestDigest::from_bytes(&stored.bytes);
                let validated = self.validate_item_contents(
                    TrashArea::Items,
                    id,
                    &items_directory,
                    &digest,
                    PayloadPresence::Optional,
                )?;
                if validated.bytes != bytes {
                    return Err(CoreError::TrashManifestCollision(
                        items_manifest_relative.as_path().to_owned(),
                    ));
                }
                return Ok(PrepareManifestOutcome::AlreadyPublished);
            }
            (true, false) => {
                let held_directory = self.open_trash_item_directory(
                    TrashArea::Staging,
                    id,
                    &staging_parent,
                    &item_name,
                )?;
                let outcome = Self::confirm_existing_manifest(
                    id,
                    &held_directory,
                    &manifest_relative,
                    &bytes,
                    &mut inject_sync,
                )?;
                let directory = self.reopen_bound_trash_item_directory(
                    TrashArea::Staging,
                    id,
                    &staging_parent,
                    &item_name,
                    &held_directory,
                )?;
                if Self::entry_exists(&items_parent, &item_name)? {
                    return Err(CoreError::InvalidTrashTopology(
                        "items directory appeared while confirming staging manifest",
                    ));
                }
                let digest = ManifestDigest::from_bytes(&bytes);
                self.validate_item_contents(
                    TrashArea::Staging,
                    id,
                    &directory,
                    &digest,
                    PayloadPresence::Optional,
                )?;
                return Ok(outcome);
            }
            (false, false) => self.ensure_directory(
                &staging_parent,
                &id.to_string(),
                &format!(".trash/v1/staging/{id}"),
            )?,
        };

        match directory.symlink_metadata("manifest.json") {
            Ok(_) => {
                return Self::confirm_existing_manifest(
                    id,
                    &directory,
                    &manifest_relative,
                    &bytes,
                    &mut inject_sync,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let (temporary_name, mut temporary) = Self::create_manifest_temp(&directory)?;
        temporary.write_all(&bytes)?;
        temporary.sync_all()?;
        Self::require_single_link_regular(&temporary, "manifest temp must be single-link file")?;
        drop(temporary);

        match rename_noreplace(
            &directory,
            &temporary_name,
            &directory,
            OsStr::new("manifest.json"),
        ) {
            Ok(()) => Self::confirm_published_manifest(
                id,
                &directory,
                &manifest_relative,
                &bytes,
                &mut inject_sync,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::confirm_existing_manifest(
                    id,
                    &directory,
                    &manifest_relative,
                    &bytes,
                    &mut inject_sync,
                )
            }
            Err(error) => Err(Vault::map_atomic_move_error(
                &VaultPath::from_portable(format!(
                    ".trash/v1/staging/{id}/{temporary_name}",
                    temporary_name = temporary_name.to_string_lossy()
                ))?,
                &manifest_relative,
                error,
            )),
        }
    }

    fn confirm_existing_manifest<F>(
        id: TrashId,
        directory: &Dir,
        relative: &VaultPath,
        expected: &[u8],
        inject_sync: &mut F,
    ) -> Result<PrepareManifestOutcome>
    where
        F: FnMut(ManifestSyncStage) -> std::io::Result<()>,
    {
        Self::sync_manifest_directory(
            directory,
            ManifestSyncStage::ExistingDirectoryPrecheck,
            inject_sync,
        )
        .map_err(|cause| Self::manifest_outcome_unknown(relative, cause))?;
        let existing = Self::read_manifest_from_directory(id, directory, true)?;
        if existing.bytes != expected {
            return Err(CoreError::TrashManifestCollision(
                relative.as_path().to_owned(),
            ));
        }
        Self::sync_manifest_file(
            &existing.file,
            ManifestSyncStage::ExistingFileDurable,
            inject_sync,
        )
        .map_err(|cause| Self::manifest_outcome_unknown(relative, cause))?;
        Self::sync_manifest_directory(
            directory,
            ManifestSyncStage::ExistingDirectoryDurable,
            inject_sync,
        )
        .map_err(|cause| Self::manifest_outcome_unknown(relative, cause))?;
        Ok(PrepareManifestOutcome::AlreadyPrepared)
    }

    fn confirm_published_manifest<F>(
        id: TrashId,
        directory: &Dir,
        relative: &VaultPath,
        expected: &[u8],
        inject_sync: &mut F,
    ) -> Result<PrepareManifestOutcome>
    where
        F: FnMut(ManifestSyncStage) -> std::io::Result<()>,
    {
        Self::sync_manifest_directory(
            directory,
            ManifestSyncStage::PublishedDirectoryPrecheck,
            inject_sync,
        )
        .map_err(|cause| Self::manifest_outcome_unknown(relative, cause))?;
        let published = Self::read_manifest_from_directory(id, directory, true)
            .map_err(|cause| Self::manifest_outcome_unknown(relative, cause))?;
        if published.bytes != expected {
            return Err(Self::manifest_outcome_unknown(
                relative,
                CoreError::TrashManifestCollision(relative.as_path().to_owned()),
            ));
        }
        Self::sync_manifest_file(
            &published.file,
            ManifestSyncStage::PublishedFileDurable,
            inject_sync,
        )
        .map_err(|cause| Self::manifest_outcome_unknown(relative, cause))?;
        Self::sync_manifest_directory(
            directory,
            ManifestSyncStage::PublishedDirectoryDurable,
            inject_sync,
        )
        .map_err(|cause| Self::manifest_outcome_unknown(relative, cause))?;
        Ok(PrepareManifestOutcome::Prepared)
    }

    fn sync_manifest_directory<F>(
        directory: &Dir,
        stage: ManifestSyncStage,
        inject_sync: &mut F,
    ) -> Result<()>
    where
        F: FnMut(ManifestSyncStage) -> std::io::Result<()>,
    {
        inject_sync(stage)?;
        sync_directory(directory)
    }

    fn sync_manifest_file<F>(
        file: &cap_std::fs::File,
        stage: ManifestSyncStage,
        inject_sync: &mut F,
    ) -> Result<()>
    where
        F: FnMut(ManifestSyncStage) -> std::io::Result<()>,
    {
        inject_sync(stage)?;
        file.sync_all().map_err(Into::into)
    }

    fn manifest_outcome_unknown(relative: &VaultPath, cause: CoreError) -> CoreError {
        CoreError::TrashManifestOutcomeUnknown {
            path: relative.as_path().to_owned(),
            cause: Box::new(cause),
        }
    }

    /// Reads one bounded, byte-for-byte canonical manifest.
    ///
    /// # Errors
    /// Returns an error for missing, oversized, noncanonical, semantically
    /// invalid, symlinked, nonregular, or multiply-linked files.
    pub fn read_manifest(&self, area: TrashArea, id: TrashId) -> Result<TrashManifestV1> {
        let relative = manifest_path(area, id)?;
        let (directory, name) = self.vault.open_parent_checked(&relative)?;
        if name != OsStr::new("manifest.json") {
            return Err(CoreError::InvalidTrashPath(relative.as_path().to_owned()));
        }
        Self::read_manifest_from_directory(id, &directory, false).map(|read| read.manifest)
    }

    /// Stages a payload using only the source/revision bound by the canonical
    /// manifest identified by `manifest_digest`.
    ///
    /// # Errors
    /// Returns an error for digest/source mismatch, stale or oversized source,
    /// hard links, collisions, symlinks, or move durability uncertainty.
    pub fn stage_payload_if_revision(
        &self,
        id: TrashId,
        source: &VaultPath,
        manifest_digest: &ManifestDigest,
    ) -> Result<StagePayloadOutcome> {
        self.stage_payload_with_hooks(
            id,
            source,
            manifest_digest,
            |_, directory| sync_directory_for_move(directory),
            || {},
        )
    }

    fn stage_payload_with_hooks<F, G>(
        &self,
        id: TrashId,
        source: &VaultPath,
        manifest_digest: &ManifestDigest,
        mut sync: F,
        after_move: G,
    ) -> Result<StagePayloadOutcome>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
        G: FnOnce(),
    {
        let _guard = self.vault.lock_mutations()?;
        Vault::require_content_path(source)?;
        let (source_parent, source_name) = self.vault.open_parent_checked(source)?;
        let staging_path = item_directory_path(TrashArea::Staging, id)?;
        let items_path = item_directory_path(TrashArea::Items, id)?;
        let (staging_parent, staging_name) = self.vault.open_parent_checked(&staging_path)?;
        let (items_parent, items_name) = self.vault.open_parent_checked(&items_path)?;
        let destination = payload_path(TrashArea::Staging, id)?;
        let container_report = MoveSyncReport {
            destination: sync(MoveSyncParent::Destination, &items_parent),
            source: Some(sync(MoveSyncParent::Source, &staging_parent)),
        };
        if container_report.has_failure() {
            return Err(container_report.into_verified_unknown(
                &staging_path,
                &items_path,
                CoreError::MoveDurabilitySyncFailed,
            ));
        }
        let container_durability = container_report.into_result(&staging_path, &items_path)?;
        let staging_exists = Self::entry_exists(&staging_parent, &staging_name)?;
        let items_exists = Self::entry_exists(&items_parent, &items_name)?;
        match (staging_exists, items_exists) {
            (true, true) => Err(CoreError::InvalidTrashTopology(
                "staging and items directories both exist while staging payload",
            )),
            (false, false) => Err(CoreError::InvalidTrashTopology(
                "staging and items directories are both absent while staging payload",
            )),
            (true, false) => self.stage_from_staging_directory(
                id,
                source,
                manifest_digest,
                &destination,
                &source_parent,
                &source_name,
                &staging_parent,
                &staging_name,
                &items_parent,
                &items_name,
                &mut sync,
                after_move,
                container_durability,
            ),
            (false, true) => {
                let published_destination = payload_path(TrashArea::Items, id)?;
                self.confirm_already_published_stage(
                    id,
                    source,
                    manifest_digest,
                    &published_destination,
                    &source_parent,
                    &source_name,
                    &items_parent,
                    &items_name,
                    &staging_parent,
                    &staging_name,
                    &mut sync,
                    container_durability,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn stage_from_staging_directory<F, G>(
        &self,
        id: TrashId,
        source: &VaultPath,
        digest: &ManifestDigest,
        destination: &VaultPath,
        source_parent: &Dir,
        source_name: &OsString,
        staging_parent: &Dir,
        staging_name: &OsString,
        items_parent: &Dir,
        items_name: &OsString,
        sync: &mut F,
        after_move: G,
        container_durability: MoveDurability,
    ) -> Result<StagePayloadOutcome>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
        G: FnOnce(),
    {
        let held =
            self.open_trash_item_directory(TrashArea::Staging, id, staging_parent, staging_name)?;
        let report = MoveSyncReport {
            destination: sync(MoveSyncParent::Destination, &held),
            source: Some(sync(MoveSyncParent::Source, source_parent)),
        };
        let authoritative = match self.reopen_bound_trash_item_directory(
            TrashArea::Staging,
            id,
            staging_parent,
            staging_name,
            &held,
        ) {
            Ok(directory) => directory,
            Err(cause) => return Err(report.into_verified_unknown(source, destination, cause)),
        };
        let source_exists = Self::entry_exists(source_parent, source_name);
        let payload_name = OsString::from("payload");
        let payload_exists = Self::entry_exists(&authoritative, &payload_name);
        let (source_exists, payload_exists) = match source_exists.and_then(|source_exists| {
            payload_exists.map(|payload_exists| (source_exists, payload_exists))
        }) {
            Ok(value) => value,
            Err(cause) => return Err(report.into_verified_unknown(source, destination, cause)),
        };
        match (source_exists, payload_exists) {
            (true, true) => Err(CoreError::AlreadyExists(destination.as_path().to_owned())),
            (false, false) => {
                let validated = self.validate_item_contents(
                    TrashArea::Staging,
                    id,
                    &authoritative,
                    digest,
                    PayloadPresence::Absent,
                )?;
                Self::require_stage_source_binding(&validated, source)?;
                Err(CoreError::InvalidTrashTopology(
                    "content and staging payload are both absent",
                ))
            }
            (false, true) => self.finish_stage_confirmation(
                TrashArea::Staging,
                id,
                source,
                digest,
                destination,
                &authoritative,
                source_parent,
                staging_parent,
                staging_name,
                items_parent,
                items_name,
                report,
                true,
                container_durability,
            ),
            (true, false) => {
                if report.has_failure() {
                    return Err(report.into_stage_prepublication_sync_failed(
                        source,
                        destination,
                        CoreError::MoveDurabilitySyncFailed,
                    ));
                }
                let validated = self.validate_item_contents(
                    TrashArea::Staging,
                    id,
                    &authoritative,
                    digest,
                    PayloadPresence::Absent,
                )?;
                Self::require_stage_source_binding(&validated, source)?;
                let expected = validated.manifest.expected_revision()?;
                self.vault.verify_expected_from_parent(
                    source_parent,
                    source_name,
                    source,
                    &expected,
                    MAX_TRASH_PAYLOAD_BYTES,
                )?;
                self.vault
                    .verify_single_link_from_parent(source_parent, source_name, source)?;
                let move_report = self.vault.atomic_move_locked_report(
                    source,
                    destination,
                    |parent, directory| sync(parent, directory),
                )?;
                after_move();
                if move_report.has_failure() {
                    let confirmation_destination = match self.open_trash_item_directory(
                        TrashArea::Staging,
                        id,
                        staging_parent,
                        staging_name,
                    ) {
                        Ok(directory) => directory,
                        Err(cause) => {
                            let source_status =
                                directory_sync_status(sync(MoveSyncParent::Source, source_parent));
                            return Err(Vault::verified_move_unknown(
                                source,
                                destination,
                                DirectorySyncStatus::NotAttempted,
                                source_status,
                                cause,
                            ));
                        }
                    };
                    let confirmation_report = MoveSyncReport {
                        destination: sync(MoveSyncParent::Destination, &confirmation_destination),
                        source: Some(sync(MoveSyncParent::Source, source_parent)),
                    };
                    if let Err(cause) = Self::require_same_directory_identity(
                        &authoritative,
                        &confirmation_destination,
                    ) {
                        return Err(confirmation_report.into_verified_unknown(
                            source,
                            destination,
                            cause,
                        ));
                    }
                    return self.finish_stage_confirmation(
                        TrashArea::Staging,
                        id,
                        source,
                        digest,
                        destination,
                        &authoritative,
                        source_parent,
                        staging_parent,
                        staging_name,
                        items_parent,
                        items_name,
                        confirmation_report,
                        false,
                        container_durability,
                    );
                }
                self.finish_stage_confirmation(
                    TrashArea::Staging,
                    id,
                    source,
                    digest,
                    destination,
                    &authoritative,
                    source_parent,
                    staging_parent,
                    staging_name,
                    items_parent,
                    items_name,
                    move_report,
                    false,
                    container_durability,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm_already_published_stage<F>(
        &self,
        id: TrashId,
        source: &VaultPath,
        digest: &ManifestDigest,
        destination: &VaultPath,
        source_parent: &Dir,
        source_name: &OsString,
        items_parent: &Dir,
        items_name: &OsString,
        staging_parent: &Dir,
        staging_name: &OsString,
        sync: &mut F,
        container_durability: MoveDurability,
    ) -> Result<StagePayloadOutcome>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
    {
        let held =
            self.open_trash_item_directory(TrashArea::Items, id, items_parent, items_name)?;
        let report = MoveSyncReport {
            destination: sync(MoveSyncParent::Destination, &held),
            source: Some(sync(MoveSyncParent::Source, source_parent)),
        };
        let authoritative = match self.reopen_bound_trash_item_directory(
            TrashArea::Items,
            id,
            items_parent,
            items_name,
            &held,
        ) {
            Ok(directory) => directory,
            Err(cause) => return Err(report.into_verified_unknown(source, destination, cause)),
        };
        let source_exists = match Self::entry_exists(source_parent, source_name) {
            Ok(exists) => exists,
            Err(cause) => return Err(report.into_verified_unknown(source, destination, cause)),
        };
        if source_exists {
            return Err(CoreError::InvalidTrashTopology(
                "content and published item both exist",
            ));
        }
        self.finish_stage_confirmation(
            TrashArea::Items,
            id,
            source,
            digest,
            destination,
            &authoritative,
            source_parent,
            items_parent,
            items_name,
            staging_parent,
            staging_name,
            report,
            true,
            container_durability,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_stage_confirmation(
        &self,
        area: TrashArea,
        id: TrashId,
        source: &VaultPath,
        digest: &ManifestDigest,
        destination: &VaultPath,
        held: &Dir,
        synced_source_parent: &Dir,
        authoritative_parent: &Dir,
        authoritative_name: &OsString,
        other_parent: &Dir,
        other_name: &OsString,
        report: MoveSyncReport,
        already: bool,
        container_durability: MoveDurability,
    ) -> Result<StagePayloadOutcome> {
        let verification = (|| {
            let (authoritative_source_parent, source_name) =
                self.vault.open_parent_checked(source)?;
            Self::require_same_directory_identity(
                synced_source_parent,
                &authoritative_source_parent,
            )?;
            if Self::entry_exists(&authoritative_source_parent, &source_name)? {
                return Err(CoreError::InvalidTrashTopology(
                    "content source exists after payload publication",
                ));
            }
            let authoritative = self.reopen_bound_trash_item_directory(
                area,
                id,
                authoritative_parent,
                authoritative_name,
                held,
            )?;
            if Self::entry_exists(other_parent, other_name)? {
                return Err(CoreError::InvalidTrashTopology(
                    "staging and items directories both exist",
                ));
            }
            let validated = self.validate_item_contents(
                area,
                id,
                &authoritative,
                digest,
                PayloadPresence::Required,
            )?;
            Self::require_stage_source_binding(&validated, source)
        })();
        if let Err(cause) = verification {
            return Err(report.into_verified_unknown(source, destination, cause));
        }
        if report.has_failure() {
            return Err(report.into_verified_unknown(
                source,
                destination,
                CoreError::MoveDurabilitySyncFailed,
            ));
        }
        let durability = container_durability.combine(report.into_result(source, destination)?);
        Ok(match (area, already) {
            (TrashArea::Staging, false) => StagePayloadOutcome::Staged(durability),
            (TrashArea::Staging, true) => StagePayloadOutcome::AlreadyStaged(durability),
            (TrashArea::Items, true) => StagePayloadOutcome::AlreadyPublished(durability),
            (TrashArea::Items, false) => {
                return Err(CoreError::InvalidTrashTopology(
                    "items cannot be a newly staged payload",
                ));
            }
        })
    }

    fn require_stage_source_binding(
        validated: &ValidatedTrashItem,
        source: &VaultPath,
    ) -> Result<()> {
        if source.as_str() == validated.manifest.original_path {
            Ok(())
        } else {
            Err(CoreError::InvalidTrashManifest(
                "payload source does not match original path",
            ))
        }
    }

    /// Atomically publishes a complete staging UUID directory into `items`.
    ///
    /// # Errors
    /// Returns a precondition error before rename, or an explicit verified
    /// move outcome error for every failure after publication.
    pub fn publish_staging_item(
        &self,
        id: TrashId,
        digest: &ManifestDigest,
    ) -> Result<PublishItemOutcome> {
        self.publish_staging_item_with_hooks(
            id,
            digest,
            |_, directory| sync_directory_for_move(directory),
            || {},
        )
    }

    fn publish_staging_item_with_hooks<F, G>(
        &self,
        id: TrashId,
        digest: &ManifestDigest,
        sync: F,
        after_move: G,
    ) -> Result<PublishItemOutcome>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
        G: FnOnce(),
    {
        self.publish_staging_item_with_observation_hooks(id, digest, sync, || Ok(()), after_move)
    }

    fn publish_staging_item_with_observation_hooks<F, H, G>(
        &self,
        id: TrashId,
        digest: &ManifestDigest,
        mut sync: F,
        before_initial_observation: H,
        after_move: G,
    ) -> Result<PublishItemOutcome>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
        H: FnOnce() -> Result<()>,
        G: FnOnce(),
    {
        let _guard = self.vault.lock_mutations()?;
        let staging_path = item_directory_path(TrashArea::Staging, id)?;
        let items_path = item_directory_path(TrashArea::Items, id)?;
        let (staging_parent, staging_name) = self.vault.open_parent_checked(&staging_path)?;
        let (items_parent, items_name) = self.vault.open_parent_checked(&items_path)?;
        let initial_report = MoveSyncReport {
            destination: sync(MoveSyncParent::Destination, &items_parent),
            source: Some(sync(MoveSyncParent::Source, &staging_parent)),
        };
        let observed = before_initial_observation().and_then(|()| {
            Ok((
                Self::entry_exists(&staging_parent, &staging_name)?,
                Self::entry_exists(&items_parent, &items_name)?,
            ))
        });
        let (staging_exists, items_exists) = match observed {
            Ok(topology) => topology,
            Err(cause) => {
                return Err(initial_report.into_verified_unknown(
                    &staging_path,
                    &items_path,
                    cause,
                ));
            }
        };

        match (staging_exists, items_exists) {
            (false, true) => self.confirm_published_item(
                id,
                digest,
                &staging_path,
                &items_path,
                &staging_parent,
                &staging_name,
                &items_parent,
                &items_name,
                None,
                initial_report,
                true,
            ),
            (true, true) => Err(CoreError::InvalidTrashTopology(
                "staging and items directories both exist",
            )),
            (false, false) => Err(CoreError::InvalidTrashTopology(
                "staging and items directories are both absent",
            )),
            (true, false) => {
                let staging_display = self.vault.root_path.join(staging_path.as_path());
                let staged_directory =
                    open_child_dir_nofollow(&staging_parent, &staging_name, &staging_display)?;
                self.validate_item_contents(
                    TrashArea::Staging,
                    id,
                    &staged_directory,
                    digest,
                    PayloadPresence::Required,
                )?;
                let report = self.vault.atomic_move_locked_report(
                    &staging_path,
                    &items_path,
                    |parent, directory| sync(parent, directory),
                )?;
                after_move();
                if report.has_failure() {
                    let confirmation_report = MoveSyncReport {
                        destination: sync(MoveSyncParent::Destination, &items_parent),
                        source: Some(sync(MoveSyncParent::Source, &staging_parent)),
                    };
                    return self.confirm_published_item(
                        id,
                        digest,
                        &staging_path,
                        &items_path,
                        &staging_parent,
                        &staging_name,
                        &items_parent,
                        &items_name,
                        Some(&staged_directory),
                        confirmation_report,
                        false,
                    );
                }
                if let Err(cause) = self.validate_published_item_topology(
                    id,
                    digest,
                    &staging_parent,
                    &staging_name,
                    &items_parent,
                    &items_name,
                    Some(&staged_directory),
                ) {
                    return Err(report.into_verified_unknown(&staging_path, &items_path, cause));
                }
                report
                    .into_result(&staging_path, &items_path)
                    .map(PublishItemOutcome::Published)
            }
        }
    }

    /// Restores an item payload only to the manifest's original content path.
    /// The immutable manifest and UUID directory remain in `items`.
    ///
    /// # Errors
    /// Returns a precondition error before rename, or an explicit verified
    /// move outcome error for every failure after publication.
    pub fn restore_item_if_revision(
        &self,
        id: TrashId,
        destination: &VaultPath,
        digest: &ManifestDigest,
    ) -> Result<RestoreItemOutcome> {
        self.restore_item_with_hooks(
            id,
            destination,
            digest,
            |_, directory| sync_directory_for_move(directory),
            || {},
        )
    }

    fn restore_item_with_hooks<F, G>(
        &self,
        id: TrashId,
        destination: &VaultPath,
        digest: &ManifestDigest,
        sync: F,
        after_move: G,
    ) -> Result<RestoreItemOutcome>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
        G: FnOnce(),
    {
        self.restore_item_with_observation_hooks(
            id,
            destination,
            digest,
            sync,
            || Ok(()),
            after_move,
        )
    }

    #[allow(clippy::too_many_lines)]
    fn restore_item_with_observation_hooks<F, H, G>(
        &self,
        id: TrashId,
        destination: &VaultPath,
        digest: &ManifestDigest,
        mut sync: F,
        before_initial_observation: H,
        after_move: G,
    ) -> Result<RestoreItemOutcome>
    where
        F: FnMut(MoveSyncParent, &Dir) -> std::io::Result<MoveDurability>,
        H: FnOnce() -> Result<()>,
        G: FnOnce(),
    {
        let _guard = self.vault.lock_mutations()?;
        Vault::require_content_path(destination)?;
        let item_path = item_directory_path(TrashArea::Items, id)?;
        let (items_parent, item_name) = self.vault.open_parent_checked(&item_path)?;
        let source = payload_path(TrashArea::Items, id)?;
        let (item_directory, payload_name) = self.vault.open_parent_checked(&source)?;
        let (destination_parent, destination_name) = self.vault.open_parent_checked(destination)?;
        let initial_report = MoveSyncReport {
            destination: sync(MoveSyncParent::Destination, &destination_parent),
            source: Some(sync(MoveSyncParent::Source, &item_directory)),
        };
        let observed = before_initial_observation().and_then(|()| {
            let authoritative = self.reopen_authoritative_item_directory(
                id,
                &items_parent,
                &item_name,
                &item_directory,
            )?;
            Ok((
                Self::entry_exists(&authoritative, &payload_name)?,
                Self::entry_exists(&destination_parent, &destination_name)?,
                authoritative,
            ))
        });
        let (payload_exists, destination_exists, authoritative_item_directory) = match observed {
            Ok(topology) => topology,
            Err(cause) => {
                return Err(initial_report.into_verified_unknown(&source, destination, cause));
            }
        };

        match (payload_exists, destination_exists) {
            (true, true) => Err(CoreError::AlreadyExists(destination.as_path().to_owned())),
            (false, false) => Err(CoreError::InvalidTrashTopology(
                "payload and restore destination are both absent",
            )),
            (false, true) => self.confirm_restored_item(
                id,
                destination,
                digest,
                None,
                &source,
                &items_parent,
                &item_name,
                &authoritative_item_directory,
                &payload_name,
                &destination_parent,
                &destination_name,
                initial_report,
                true,
            ),
            (true, false) => {
                let validated = self.validate_item_contents(
                    TrashArea::Items,
                    id,
                    &authoritative_item_directory,
                    digest,
                    PayloadPresence::Required,
                )?;
                if destination.as_str() != validated.manifest.original_path {
                    return Err(CoreError::InvalidTrashManifest(
                        "restore destination must equal original path",
                    ));
                }
                self.vault.reject_sibling_collision(
                    &destination_parent,
                    destination_name.to_str().ok_or_else(|| {
                        CoreError::InvalidRelativePath(destination.as_path().to_owned())
                    })?,
                    destination,
                )?;
                let expected = validated.manifest.expected_revision()?;
                self.vault.verify_expected_from_parent(
                    &authoritative_item_directory,
                    &payload_name,
                    &source,
                    &expected,
                    MAX_TRASH_PAYLOAD_BYTES,
                )?;
                self.vault.verify_single_link_from_parent(
                    &authoritative_item_directory,
                    &payload_name,
                    &source,
                )?;
                let report = self.vault.atomic_move_locked_report(
                    &source,
                    destination,
                    |parent, directory| sync(parent, directory),
                )?;
                after_move();
                if report.has_failure() {
                    let confirmation_destination =
                        sync(MoveSyncParent::Destination, &destination_parent);
                    let confirmation_source =
                        match self.open_authoritative_item_directory(id, &items_parent, &item_name)
                        {
                            Ok(directory) => directory,
                            Err(cause) => {
                                return Err(Vault::verified_move_unknown(
                                    &source,
                                    destination,
                                    directory_sync_status(confirmation_destination),
                                    DirectorySyncStatus::NotAttempted,
                                    cause,
                                ));
                            }
                        };
                    let confirmation_report = MoveSyncReport {
                        destination: confirmation_destination,
                        source: Some(sync(MoveSyncParent::Source, &confirmation_source)),
                    };
                    if let Err(cause) = Self::require_same_directory_identity(
                        &authoritative_item_directory,
                        &confirmation_source,
                    ) {
                        return Err(confirmation_report.into_verified_unknown(
                            &source,
                            destination,
                            cause,
                        ));
                    }
                    return self.confirm_restored_item(
                        id,
                        destination,
                        digest,
                        Some(&validated),
                        &source,
                        &items_parent,
                        &item_name,
                        &authoritative_item_directory,
                        &payload_name,
                        &destination_parent,
                        &destination_name,
                        confirmation_report,
                        false,
                    );
                }
                if let Err(cause) = self.validate_restored_topology(
                    id,
                    destination,
                    digest,
                    &validated,
                    &items_parent,
                    &item_name,
                    &authoritative_item_directory,
                    &payload_name,
                    &destination_parent,
                    &destination_name,
                ) {
                    return Err(report.into_verified_unknown(&source, destination, cause));
                }
                report
                    .into_result(&source, destination)
                    .map(RestoreItemOutcome::Restored)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm_published_item(
        &self,
        id: TrashId,
        digest: &ManifestDigest,
        staging_path: &VaultPath,
        items_path: &VaultPath,
        staging_parent: &Dir,
        staging_name: &OsString,
        items_parent: &Dir,
        items_name: &OsString,
        expected_items_directory: Option<&Dir>,
        report: MoveSyncReport,
        already_published: bool,
    ) -> Result<PublishItemOutcome> {
        if let Err(cause) = self.validate_published_item_topology(
            id,
            digest,
            staging_parent,
            staging_name,
            items_parent,
            items_name,
            expected_items_directory,
        ) {
            return Err(report.into_verified_unknown(staging_path, items_path, cause));
        }
        if report.has_failure() {
            return Err(report.into_verified_unknown(
                staging_path,
                items_path,
                CoreError::MoveDurabilitySyncFailed,
            ));
        }
        let durability = report.into_result(staging_path, items_path)?;
        Ok(if already_published {
            PublishItemOutcome::AlreadyPublished(durability)
        } else {
            PublishItemOutcome::Published(durability)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm_restored_item(
        &self,
        id: TrashId,
        destination: &VaultPath,
        digest: &ManifestDigest,
        validated: Option<&ValidatedTrashItem>,
        source: &VaultPath,
        items_parent: &Dir,
        item_name: &OsString,
        held_item_directory: &Dir,
        payload_name: &OsString,
        destination_parent: &Dir,
        destination_name: &OsString,
        report: MoveSyncReport,
        already_restored: bool,
    ) -> Result<RestoreItemOutcome> {
        let verification = if let Some(validated) = validated {
            self.validate_restored_topology(
                id,
                destination,
                digest,
                validated,
                items_parent,
                item_name,
                held_item_directory,
                payload_name,
                destination_parent,
                destination_name,
            )
        } else {
            self.validate_completed_restore_topology(
                id,
                destination,
                digest,
                items_parent,
                item_name,
                held_item_directory,
                destination_parent,
                destination_name,
            )
        };
        if let Err(cause) = verification {
            return Err(report.into_verified_unknown(source, destination, cause));
        }
        if report.has_failure() {
            return Err(report.into_verified_unknown(
                source,
                destination,
                CoreError::MoveDurabilitySyncFailed,
            ));
        }
        let durability = report.into_result(source, destination)?;
        Ok(if already_restored {
            RestoreItemOutcome::AlreadyRestored(durability)
        } else {
            RestoreItemOutcome::Restored(durability)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_published_item_topology(
        &self,
        id: TrashId,
        digest: &ManifestDigest,
        staging_parent: &Dir,
        staging_name: &OsString,
        items_parent: &Dir,
        items_name: &OsString,
        expected_items_directory: Option<&Dir>,
    ) -> Result<()> {
        if Self::entry_exists(staging_parent, staging_name)? {
            return Err(CoreError::InvalidTrashTopology(
                "staging directory was recreated after publish",
            ));
        }
        if let Some(expected) = expected_items_directory {
            let authoritative =
                self.reopen_authoritative_item_directory(id, items_parent, items_name, expected)?;
            self.validate_item_contents(
                TrashArea::Items,
                id,
                &authoritative,
                digest,
                PayloadPresence::Required,
            )?;
        } else {
            self.validate_item_directory(
                TrashArea::Items,
                id,
                items_parent,
                items_name,
                digest,
                PayloadPresence::Required,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_restored_topology(
        &self,
        id: TrashId,
        destination: &VaultPath,
        digest: &ManifestDigest,
        validated: &ValidatedTrashItem,
        items_parent: &Dir,
        item_name: &OsString,
        held_item_directory: &Dir,
        payload_name: &OsString,
        destination_parent: &Dir,
        destination_name: &OsString,
    ) -> Result<()> {
        let item_directory = self.reopen_authoritative_item_directory(
            id,
            items_parent,
            item_name,
            held_item_directory,
        )?;
        let expected = validated.manifest.expected_revision()?;
        self.vault.verify_expected_from_parent(
            destination_parent,
            destination_name,
            destination,
            &expected,
            MAX_TRASH_PAYLOAD_BYTES,
        )?;
        self.vault.verify_single_link_from_parent(
            destination_parent,
            destination_name,
            destination,
        )?;
        if Self::entry_exists(&item_directory, payload_name)? {
            return Err(CoreError::InvalidTrashTopology(
                "items payload still exists after restore",
            ));
        }
        let reread = self.validate_item_contents(
            TrashArea::Items,
            id,
            &item_directory,
            digest,
            PayloadPresence::Absent,
        )?;
        if reread.bytes != validated.bytes {
            return Err(CoreError::TrashManifestDigestMismatch);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_completed_restore_topology(
        &self,
        id: TrashId,
        destination: &VaultPath,
        digest: &ManifestDigest,
        items_parent: &Dir,
        item_name: &OsString,
        held_item_directory: &Dir,
        destination_parent: &Dir,
        destination_name: &OsString,
    ) -> Result<()> {
        let item_directory = self.reopen_authoritative_item_directory(
            id,
            items_parent,
            item_name,
            held_item_directory,
        )?;
        let validated = self.validate_item_contents(
            TrashArea::Items,
            id,
            &item_directory,
            digest,
            PayloadPresence::Absent,
        )?;
        if destination.as_str() != validated.manifest.original_path {
            return Err(CoreError::InvalidTrashManifest(
                "restore destination must equal original path",
            ));
        }
        let expected = validated.manifest.expected_revision()?;
        self.vault.verify_expected_from_parent(
            destination_parent,
            destination_name,
            destination,
            &expected,
            MAX_TRASH_PAYLOAD_BYTES,
        )?;
        self.vault
            .verify_single_link_from_parent(destination_parent, destination_name, destination)
    }

    fn reopen_authoritative_item_directory(
        &self,
        id: TrashId,
        items_parent: &Dir,
        item_name: &OsString,
        held_item_directory: &Dir,
    ) -> Result<Dir> {
        let authoritative = self.open_authoritative_item_directory(id, items_parent, item_name)?;
        Self::require_same_directory_identity(held_item_directory, &authoritative)?;
        Ok(authoritative)
    }

    fn open_authoritative_item_directory(
        &self,
        id: TrashId,
        items_parent: &Dir,
        item_name: &OsString,
    ) -> Result<Dir> {
        let item_path = item_directory_path(TrashArea::Items, id)?;
        let display = self.vault.root_path.join(item_path.as_path());
        open_child_dir_nofollow(items_parent, item_name, &display)
    }

    fn open_trash_item_directory(
        &self,
        area: TrashArea,
        id: TrashId,
        parent: &Dir,
        name: &OsString,
    ) -> Result<Dir> {
        let path = item_directory_path(area, id)?;
        let display = self.vault.root_path.join(path.as_path());
        open_child_dir_nofollow(parent, name, &display)
    }

    fn reopen_bound_trash_item_directory(
        &self,
        area: TrashArea,
        id: TrashId,
        parent: &Dir,
        name: &OsString,
        held: &Dir,
    ) -> Result<Dir> {
        let authoritative = self.open_trash_item_directory(area, id, parent, name)?;
        Self::require_same_directory_identity(held, &authoritative)?;
        Ok(authoritative)
    }

    fn require_same_directory_identity(expected: &Dir, actual: &Dir) -> Result<()> {
        let expected_identity = myvault_platform_fs::directory_identity(expected)?;
        let actual_identity = myvault_platform_fs::directory_identity(actual)?;
        if expected_identity != actual_identity {
            return Err(CoreError::InvalidTrashTopology(
                "trash item directory identity changed",
            ));
        }
        Ok(())
    }

    fn validate_item_directory(
        &self,
        area: TrashArea,
        id: TrashId,
        parent: &Dir,
        name: &OsString,
        digest: &ManifestDigest,
        payload_presence: PayloadPresence,
    ) -> Result<ValidatedTrashItem> {
        let path = item_directory_path(area, id)?;
        let display = self.vault.root_path.join(path.as_path());
        let directory = open_child_dir_nofollow(parent, name, &display)?;
        self.validate_item_contents(area, id, &directory, digest, payload_presence)
    }

    fn validate_item_contents(
        &self,
        area: TrashArea,
        id: TrashId,
        directory: &Dir,
        digest: &ManifestDigest,
        payload_presence: PayloadPresence,
    ) -> Result<ValidatedTrashItem> {
        let mut payload_seen = false;
        let mut manifest_seen = false;
        let mut temp_count = 0_usize;
        let mut entry_count = 0_usize;
        for entry in directory.entries()? {
            let entry = entry?;
            entry_count = entry_count.saturating_add(1);
            if entry_count > MAX_TRASH_ITEM_ENTRIES {
                return Err(CoreError::InvalidTrashTopology(
                    "trash item contains too many entries",
                ));
            }
            let name = entry.file_name();
            let Some(name_utf8) = name.to_str() else {
                return Err(CoreError::InvalidTrashTopology(
                    "trash item entry name is not UTF-8",
                ));
            };
            match name_utf8 {
                "manifest.json" => manifest_seen = true,
                "payload" => payload_seen = true,
                _ if Self::is_reserved_manifest_temp(name_utf8) => {
                    temp_count = temp_count.saturating_add(1);
                    if temp_count > MAX_RESERVED_MANIFEST_TEMPS {
                        return Err(CoreError::InvalidTrashTopology(
                            "too many reserved manifest temp files",
                        ));
                    }
                    Self::validate_reserved_temp(directory, &name)?;
                }
                _ => {
                    return Err(CoreError::InvalidTrashTopology(
                        "trash item contains an unrecognized entry",
                    ));
                }
            }
        }
        if !manifest_seen {
            return Err(CoreError::InvalidTrashTopology("manifest is absent"));
        }
        match payload_presence {
            PayloadPresence::Required if !payload_seen => {
                return Err(CoreError::InvalidTrashTopology("payload is absent"));
            }
            PayloadPresence::Absent if payload_seen => {
                return Err(CoreError::InvalidTrashTopology("payload is present"));
            }
            PayloadPresence::Required | PayloadPresence::Optional | PayloadPresence::Absent => {}
        }
        let stored = Self::read_manifest_from_directory(id, directory, false)?;
        if ManifestDigest::from_bytes(&stored.bytes) != *digest {
            return Err(CoreError::TrashManifestDigestMismatch);
        }
        if payload_seen {
            let expected = stored.manifest.expected_revision()?;
            let payload_relative = payload_path(area, id)?;
            let payload_name = OsString::from("payload");
            self.vault.verify_expected_from_parent(
                directory,
                &payload_name,
                &payload_relative,
                &expected,
                MAX_TRASH_PAYLOAD_BYTES,
            )?;
            self.vault.verify_single_link_from_parent(
                directory,
                &payload_name,
                &payload_relative,
            )?;
        }
        Ok(ValidatedTrashItem {
            manifest: stored.manifest,
            bytes: stored.bytes,
        })
    }

    fn validate_reserved_temp(directory: &Dir, name: &OsString) -> Result<()> {
        let metadata = directory.symlink_metadata(name)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > u64::try_from(MAX_TRASH_MANIFEST_BYTES).unwrap_or(u64::MAX)
        {
            return Err(CoreError::InvalidTrashTopology(
                "reserved manifest temp is not a bounded regular file",
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = directory.open_with(name, &options)?;
        let opened_metadata = file.metadata()?;
        if !opened_metadata.is_file()
            || opened_metadata.nlink() != 1
            || opened_metadata.len() > u64::try_from(MAX_TRASH_MANIFEST_BYTES).unwrap_or(u64::MAX)
        {
            return Err(CoreError::InvalidTrashTopology(
                "reserved manifest temp must be a bounded single-link regular file",
            ));
        }
        Ok(())
    }

    fn is_reserved_manifest_temp(name: &str) -> bool {
        let Some(uuid) = name
            .strip_prefix(".manifest-")
            .and_then(|value| value.strip_suffix(".tmp"))
        else {
            return false;
        };
        uuid::Uuid::parse_str(uuid)
            .is_ok_and(|parsed| !parsed.is_nil() && parsed.to_string() == uuid)
    }

    fn entry_exists(parent: &Dir, name: &OsString) -> Result<bool> {
        match parent.symlink_metadata(name) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn ensure_trash_containers(&self) -> Result<(Dir, Dir)> {
        let root = self.vault.root_dir.try_clone()?;
        let trash = self.ensure_directory(&root, ".trash", ".trash")?;
        let version = self.ensure_directory(&trash, "v1", ".trash/v1")?;
        let staging = self.ensure_directory(&version, "staging", ".trash/v1/staging")?;
        let items = self.ensure_directory(&version, "items", ".trash/v1/items")?;
        Ok((staging, items))
    }

    fn ensure_directory(&self, parent: &Dir, name: &str, portable: &str) -> Result<Dir> {
        let path = VaultPath::from_portable(portable)?;
        self.vault.reject_sibling_collision(parent, name, &path)?;
        let created = match parent.create_dir(name) {
            Ok(()) => {
                sync_directory(parent)?;
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error.into()),
        };
        let display = self.vault.root_path.join(path.as_path());
        let directory = open_child_dir_nofollow(parent, OsStr::new(name), &display)?;
        self.vault.reject_sibling_collision(parent, name, &path)?;
        if !created {
            sync_directory(parent)?;
        }
        Ok(directory)
    }

    fn create_manifest_temp(directory: &Dir) -> Result<(OsString, cap_std::fs::File)> {
        for _ in 0..16 {
            let name = OsString::from(format!(".manifest-{}.tmp", uuid::Uuid::new_v4()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            options.follow(FollowSymlinks::No);
            match directory.open_with(&name, &options) {
                Ok(file) => {
                    Self::require_single_link_regular(
                        &file,
                        "manifest temp must be single-link file",
                    )?;
                    return Ok((name, file));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "unable to allocate manifest temporary name",
        )
        .into())
    }

    fn read_manifest_from_directory(
        id: TrashId,
        directory: &Dir,
        writable: bool,
    ) -> Result<ManifestRead> {
        let metadata = directory.symlink_metadata("manifest.json")?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(CoreError::InvalidTrashManifest(
                "manifest must be a regular file",
            ));
        }
        if metadata.len() > MAX_TRASH_MANIFEST_BYTES as u64 {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "trash manifest bytes",
                limit: MAX_TRASH_MANIFEST_BYTES,
            });
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(writable)
            .follow(FollowSymlinks::No);
        let mut file = directory.open_with("manifest.json", &options)?;
        Self::require_single_link_regular(&file, "manifest must be single-link file")?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(MAX_TRASH_MANIFEST_BYTES)
                .min(MAX_TRASH_MANIFEST_BYTES),
        );
        Read::by_ref(&mut file)
            .take((MAX_TRASH_MANIFEST_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_TRASH_MANIFEST_BYTES {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "trash manifest bytes",
                limit: MAX_TRASH_MANIFEST_BYTES,
            });
        }
        let manifest: TrashManifestV1 = serde_json::from_slice(&bytes)
            .map_err(|_| CoreError::InvalidTrashManifest("manifest JSON is invalid"))?;
        manifest.validate(Some(id))?;
        if manifest.canonical_bytes()? != bytes {
            return Err(CoreError::NonCanonicalTrashManifest);
        }
        Ok(ManifestRead {
            manifest,
            bytes,
            file,
        })
    }

    fn require_single_link_regular(file: &cap_std::fs::File, reason: &'static str) -> Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(CoreError::InvalidTrashManifest(reason));
        }
        Ok(())
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManifestSyncStage {
    ExistingDirectoryPrecheck,
    ExistingFileDurable,
    ExistingDirectoryDurable,
    PublishedDirectoryPrecheck,
    PublishedFileDurable,
    PublishedDirectoryDurable,
}

struct ManifestRead {
    manifest: TrashManifestV1,
    bytes: Vec<u8>,
    file: cap_std::fs::File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PayloadPresence {
    Required,
    Optional,
    Absent,
}

struct ValidatedTrashItem {
    manifest: TrashManifestV1,
    bytes: Vec<u8>,
}

const MAX_RESERVED_MANIFEST_TEMPS: usize = 32;
const MAX_TRASH_ITEM_ENTRIES: usize = MAX_RESERVED_MANIFEST_TEMPS + 2;

#[derive(Debug)]
struct MoveSyncReport {
    destination: std::io::Result<MoveDurability>,
    source: Option<std::io::Result<MoveDurability>>,
}

impl MoveSyncReport {
    fn has_failure(&self) -> bool {
        self.destination.is_err()
            || self
                .source
                .as_ref()
                .is_some_and(std::result::Result::is_err)
    }

    fn into_result(self, source: &VaultPath, destination: &VaultPath) -> Result<MoveDurability> {
        Vault::finish_atomic_move_sync(source, destination, self.destination, self.source)
    }

    fn into_verified_unknown(
        self,
        source: &VaultPath,
        destination: &VaultPath,
        verification: CoreError,
    ) -> CoreError {
        Vault::verified_move_unknown(
            source,
            destination,
            directory_sync_status(self.destination),
            self.source.map_or(
                DirectorySyncStatus::SharedWithDestination,
                directory_sync_status,
            ),
            verification,
        )
    }

    fn into_stage_prepublication_sync_failed(
        self,
        source: &VaultPath,
        destination: &VaultPath,
        cause: CoreError,
    ) -> CoreError {
        CoreError::StagePayloadPrepublicationSyncFailed {
            source_path: source.as_path().to_owned(),
            destination_path: destination.as_path().to_owned(),
            destination_sync: directory_sync_status(self.destination),
            source_sync: self.source.map_or(
                DirectorySyncStatus::SharedWithDestination,
                directory_sync_status,
            ),
            cause: Box::new(cause),
        }
    }

    fn into_content_prepublication_sync_failed(
        self,
        source: &VaultPath,
        destination: &VaultPath,
        cause: CoreError,
    ) -> CoreError {
        CoreError::MoveContentPrepublicationSyncFailed {
            source_path: source.as_path().to_owned(),
            destination_path: destination.as_path().to_owned(),
            destination_sync: directory_sync_status(self.destination),
            source_sync: self.source.map_or(
                DirectorySyncStatus::SharedWithDestination,
                directory_sync_status,
            ),
            cause: Box::new(cause),
        }
    }
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

    fn manifest_fault_fixture(label: &str) -> (PathBuf, Vault, TrashId, TrashManifestV1) {
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let base = temp_root.join(format!(
            "myvault-manifest-fault-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).expect("vault fixture");
        let vault = Vault::open(&base).expect("open vault");
        let id = TrashId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").expect("trash id");
        let manifest = TrashManifestV1::new(
            id,
            uuid::Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").expect("operation id"),
            &VaultPath::from_portable("note.md").expect("content path"),
            FileRevision::from_bytes(b"note"),
            1,
        )
        .expect("manifest");
        (base, vault, id, manifest)
    }

    fn staged_publish_fault_fixture(
        label: &str,
    ) -> (PathBuf, Vault, TrashId, TrashManifestV1, ManifestDigest) {
        let (base, vault, id, manifest) = manifest_fault_fixture(label);
        fs::write(base.join("note.md"), b"note").expect("source");
        let digest = manifest.digest().expect("digest");
        vault
            .trash_store()
            .prepare_staging_manifest(id, &manifest)
            .expect("prepare");
        vault
            .trash_store()
            .stage_payload_if_revision(
                id,
                &VaultPath::from_portable("note.md").expect("source path"),
                &digest,
            )
            .expect("stage");
        (base, vault, id, manifest, digest)
    }

    #[test]
    fn published_manifest_sync_failure_is_outcome_unknown_before_verification() {
        let (base, vault, id, manifest) = manifest_fault_fixture("published-sync");
        let mut stages = Vec::new();
        let error = vault
            .trash_store()
            .prepare_staging_manifest_with_sync(id, &manifest, |stage| {
                stages.push(stage);
                if stage == ManifestSyncStage::PublishedDirectoryPrecheck {
                    Err(std::io::Error::other("injected directory sync failure"))
                } else {
                    Ok(())
                }
            })
            .expect_err("published directory sync failure");

        assert_eq!(stages, [ManifestSyncStage::PublishedDirectoryPrecheck]);
        assert!(matches!(
            error,
            CoreError::TrashManifestOutcomeUnknown { cause, .. }
                if matches!(*cause, CoreError::Io(_))
        ));
        assert!(base
            .join(format!(".trash/v1/staging/{id}/manifest.json"))
            .is_file());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn retry_syncs_directory_before_observing_malformed_final() {
        let (base, vault, id, manifest) = manifest_fault_fixture("malformed-retry");
        vault
            .trash_store()
            .prepare_staging_manifest(id, &manifest)
            .expect("initial prepare");
        fs::write(
            base.join(format!(".trash/v1/staging/{id}/manifest.json")),
            b"{",
        )
        .expect("malformed swap");
        let mut stages = Vec::new();

        let error = vault
            .trash_store()
            .prepare_staging_manifest_with_sync(id, &manifest, |stage| {
                stages.push(stage);
                Ok(())
            })
            .expect_err("malformed retry");

        assert_eq!(stages, [ManifestSyncStage::ExistingDirectoryPrecheck]);
        assert!(matches!(error, CoreError::InvalidTrashManifest(_)));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn exact_retry_requires_directory_then_file_then_directory_sync() {
        let (base, vault, id, manifest) = manifest_fault_fixture("exact-retry");
        vault
            .trash_store()
            .prepare_staging_manifest(id, &manifest)
            .expect("initial prepare");
        let mut stages = Vec::new();

        let outcome = vault
            .trash_store()
            .prepare_staging_manifest_with_sync(id, &manifest, |stage| {
                stages.push(stage);
                Ok(())
            })
            .expect("exact retry");

        assert_eq!(outcome, PrepareManifestOutcome::AlreadyPrepared);
        assert_eq!(
            stages,
            [
                ManifestSyncStage::ExistingDirectoryPrecheck,
                ManifestSyncStage::ExistingFileDurable,
                ManifestSyncStage::ExistingDirectoryDurable,
            ]
        );
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn prepare_retry_rejects_staging_and_items_directory_swaps() {
        for (label, publish) in [("staging", false), ("items", true)] {
            let (base, vault, id, manifest, digest) =
                staged_publish_fault_fixture(&format!("prepare-swap-{label}"));
            if publish {
                vault
                    .trash_store()
                    .publish_staging_item(id, &digest)
                    .expect("publish");
            }
            let area = if publish { "items" } else { "staging" };
            let directory = base.join(format!(".trash/v1/{area}/{id}"));
            let detached = base.join(format!(".trash/v1/{area}/{id}.detached"));
            let manifest_bytes = fs::read(directory.join("manifest.json")).expect("manifest");
            let payload_bytes = fs::read(directory.join("payload")).expect("payload");
            let mut swapped = false;

            let error = vault
                .trash_store()
                .prepare_staging_manifest_with_sync(id, &manifest, |stage| {
                    if stage == ManifestSyncStage::ExistingDirectoryPrecheck && !swapped {
                        swapped = true;
                        fs::rename(&directory, &detached).expect("detach UUID directory");
                        fs::create_dir(&directory).expect("replacement UUID directory");
                        fs::write(directory.join("manifest.json"), &manifest_bytes)
                            .expect("replacement manifest");
                        fs::write(directory.join("payload"), &payload_bytes)
                            .expect("replacement payload");
                    }
                    Ok(())
                })
                .expect_err("directory identity swap");

            assert!(swapped);
            assert!(matches!(
                error,
                CoreError::InvalidTrashTopology("trash item directory identity changed")
            ));
            fs::remove_dir_all(base).expect("cleanup");
        }
    }

    #[test]
    fn stage_reread_mismatch_after_move_is_payload_outcome_unknown() {
        for (label, injected_durability) in [
            ("fully-synced", MoveDurability::FullySynced),
            (
                "directory-sync-unsupported",
                MoveDurability::DirectorySyncUnsupported,
            ),
        ] {
            let (base, vault, id, manifest) = manifest_fault_fixture(label);
            fs::write(base.join("note.md"), b"note").expect("source");
            vault
                .trash_store()
                .prepare_staging_manifest(id, &manifest)
                .expect("prepare manifest");
            let digest = manifest.digest().expect("digest");
            let manifest_path = base.join(format!(".trash/v1/staging/{id}/manifest.json"));
            let source = VaultPath::from_portable("note.md").expect("source path");
            let destination = format!(".trash/v1/staging/{id}/payload");

            let error = vault
                .trash_store()
                .stage_payload_with_hooks(
                    id,
                    &source,
                    &digest,
                    |_, _| Ok(injected_durability),
                    || fs::write(&manifest_path, b"{").expect("external manifest mutation"),
                )
                .expect_err("post-move manifest mutation");

            let status_matches = match error {
                CoreError::VerifiedMoveOutcomeUnknown {
                    source_path,
                    destination_path,
                    destination_sync,
                    source_sync,
                    verification,
                } if source_path == source.as_path()
                    && destination_path == Path::new(&destination)
                    && matches!(*verification, CoreError::InvalidTrashManifest(_)) =>
                {
                    match injected_durability {
                        MoveDurability::FullySynced => matches!(
                            (destination_sync, source_sync),
                            (DirectorySyncStatus::Synced, DirectorySyncStatus::Synced)
                        ),
                        MoveDurability::DirectorySyncUnsupported => matches!(
                            (destination_sync, source_sync),
                            (
                                DirectorySyncStatus::Unsupported,
                                DirectorySyncStatus::Unsupported
                            )
                        ),
                    }
                }
                _ => false,
            };
            assert!(status_matches);
            assert!(base.join(&destination).is_file());
            assert!(!base.join("note.md").exists());
            fs::remove_dir_all(base).expect("cleanup");
        }
    }

    #[test]
    fn stage_syncs_destination_then_source_before_each_observation() {
        let (base, vault, id, manifest, digest) = staged_publish_fault_fixture("stage-sync-order");
        fs::rename(
            base.join(format!(".trash/v1/staging/{id}/payload")),
            base.join("note.md"),
        )
        .expect("restore fixture source");
        let source = VaultPath::from_portable(&manifest.original_path).expect("source");
        let mut attempts = Vec::new();
        let outcome = vault
            .trash_store()
            .stage_payload_with_hooks(
                id,
                &source,
                &digest,
                |parent, _| {
                    attempts.push(parent);
                    match parent {
                        MoveSyncParent::Destination => Ok(MoveDurability::DirectorySyncUnsupported),
                        MoveSyncParent::Source => Ok(MoveDurability::FullySynced),
                    }
                },
                || {},
            )
            .expect("stage");

        assert_eq!(
            attempts,
            [
                MoveSyncParent::Destination,
                MoveSyncParent::Source,
                MoveSyncParent::Destination,
                MoveSyncParent::Source,
                MoveSyncParent::Destination,
                MoveSyncParent::Source,
            ]
        );
        assert_eq!(
            outcome,
            StagePayloadOutcome::Staged(MoveDurability::DirectorySyncUnsupported)
        );
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn stage_container_sync_failures_preserve_exact_statuses_and_paths() {
        for (label, fail_destination, fail_source) in [
            ("destination", true, false),
            ("source", false, true),
            ("both", true, true),
        ] {
            let (base, vault, id, manifest, digest) =
                staged_publish_fault_fixture(&format!("stage-container-{label}"));
            let source = VaultPath::from_portable(&manifest.original_path).expect("source");
            let mut attempts = Vec::new();
            let error = vault
                .trash_store()
                .stage_payload_with_hooks(
                    id,
                    &source,
                    &digest,
                    |parent, _| {
                        attempts.push(parent);
                        if (parent == MoveSyncParent::Destination && fail_destination)
                            || (parent == MoveSyncParent::Source && fail_source)
                        {
                            Err(injected_sync_failure(parent))
                        } else {
                            Ok(MoveDurability::FullySynced)
                        }
                    },
                    || {},
                )
                .expect_err("container sync failure");

            assert_eq!(
                attempts,
                [MoveSyncParent::Destination, MoveSyncParent::Source]
            );
            assert!(matches!(
                error,
                CoreError::VerifiedMoveOutcomeUnknown {
                    source_path,
                    destination_path,
                    destination_sync,
                    source_sync,
                    verification,
                } if source_path == Path::new(&format!(".trash/v1/staging/{id}"))
                    && destination_path == Path::new(&format!(".trash/v1/items/{id}"))
                    && matches!(*verification, CoreError::MoveDurabilitySyncFailed)
                    && matches!(destination_sync, DirectorySyncStatus::Failed(_)) == fail_destination
                    && matches!(source_sync, DirectorySyncStatus::Failed(_)) == fail_source
            ));
            fs::remove_dir_all(base).expect("cleanup");
        }
    }

    #[test]
    fn stage_endpoint_sync_failure_never_publishes_or_loses_status() {
        let (base, vault, id, manifest) = manifest_fault_fixture("stage-endpoint-sync");
        fs::write(base.join("note.md"), b"note").expect("source");
        let digest = manifest.digest().expect("digest");
        vault
            .trash_store()
            .prepare_staging_manifest(id, &manifest)
            .expect("prepare");
        let source = VaultPath::from_portable(&manifest.original_path).expect("source");
        let mut call = 0_u8;
        let error = vault
            .trash_store()
            .stage_payload_with_hooks(
                id,
                &source,
                &digest,
                |parent, _| {
                    call = call.saturating_add(1);
                    if call == 3 {
                        Err(injected_sync_failure(parent))
                    } else {
                        Ok(MoveDurability::FullySynced)
                    }
                },
                || {},
            )
            .expect_err("endpoint destination sync failure");

        assert_eq!(call, 4);
        assert!(matches!(
            error,
            CoreError::StagePayloadPrepublicationSyncFailed {
                source_path,
                destination_path,
                destination_sync: DirectorySyncStatus::Failed(_),
                source_sync: DirectorySyncStatus::Synced,
                cause,
            } if source_path == source.as_path()
                && destination_path == Path::new(&format!(".trash/v1/staging/{id}/payload"))
                && matches!(*cause, CoreError::MoveDurabilitySyncFailed)
        ));
        assert!(base.join("note.md").is_file());
        assert!(!base
            .join(format!(".trash/v1/staging/{id}/payload"))
            .exists());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn stage_container_unsupported_propagates_to_all_retry_outcomes() {
        for (label, publish, expected_published) in [
            ("already-staged", false, false),
            ("already-published", true, true),
        ] {
            let (base, vault, id, manifest, digest) =
                staged_publish_fault_fixture(&format!("stage-container-{label}"));
            if publish {
                vault
                    .trash_store()
                    .publish_staging_item(id, &digest)
                    .expect("publish");
            }
            let source = VaultPath::from_portable(&manifest.original_path).expect("source");
            let mut call = 0_u8;
            let outcome = vault
                .trash_store()
                .stage_payload_with_hooks(
                    id,
                    &source,
                    &digest,
                    |_, _| {
                        call = call.saturating_add(1);
                        if call == 1 {
                            Ok(MoveDurability::DirectorySyncUnsupported)
                        } else {
                            Ok(MoveDurability::FullySynced)
                        }
                    },
                    || {},
                )
                .expect("retry outcome");

            if expected_published {
                assert_eq!(
                    outcome,
                    StagePayloadOutcome::AlreadyPublished(MoveDurability::DirectorySyncUnsupported)
                );
            } else {
                assert_eq!(
                    outcome,
                    StagePayloadOutcome::AlreadyStaged(MoveDurability::DirectorySyncUnsupported)
                );
            }
            fs::remove_dir_all(base).expect("cleanup");
        }
    }

    #[test]
    fn stage_postmove_rejects_exact_looking_directory_swap() {
        let (base, vault, id, manifest) = manifest_fault_fixture("stage-directory-swap");
        fs::write(base.join("note.md"), b"note").expect("source");
        let digest = manifest.digest().expect("digest");
        vault
            .trash_store()
            .prepare_staging_manifest(id, &manifest)
            .expect("prepare");
        let source = VaultPath::from_portable(&manifest.original_path).expect("source");
        let staging = base.join(format!(".trash/v1/staging/{id}"));
        let detached = base.join(format!(".trash/v1/staging/{id}.detached"));
        let manifest_bytes = fs::read(staging.join("manifest.json")).expect("manifest");
        let error = vault
            .trash_store()
            .stage_payload_with_hooks(
                id,
                &source,
                &digest,
                |_, _| Ok(MoveDurability::FullySynced),
                || {
                    let payload = fs::read(staging.join("payload")).expect("payload");
                    fs::rename(&staging, &detached).expect("detach staging");
                    fs::create_dir(&staging).expect("replacement staging");
                    fs::write(staging.join("manifest.json"), &manifest_bytes)
                        .expect("replacement manifest");
                    fs::write(staging.join("payload"), payload).expect("replacement payload");
                },
            )
            .expect_err("directory swap");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidTrashTopology(
                "trash item directory identity changed"
            ))
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn publish_sync_failure_confirms_with_destination_then_source() {
        let (base, vault, id, _manifest, digest) = staged_publish_fault_fixture("publish-confirm");
        let mut attempts = Vec::new();
        let mut first_destination = true;
        let outcome = vault
            .trash_store()
            .publish_staging_item_with_hooks(
                id,
                &digest,
                |parent, _| {
                    attempts.push(parent);
                    if parent == MoveSyncParent::Destination && first_destination {
                        first_destination = false;
                        Err(injected_sync_failure(parent))
                    } else {
                        Ok(MoveDurability::FullySynced)
                    }
                },
                || {},
            )
            .expect("confirmed publish");

        assert_eq!(
            attempts,
            [
                MoveSyncParent::Destination,
                MoveSyncParent::Source,
                MoveSyncParent::Destination,
                MoveSyncParent::Source,
            ]
        );
        assert_eq!(
            outcome,
            PublishItemOutcome::Published(MoveDurability::FullySynced)
        );
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn publish_idempotent_confirmation_syncs_before_rejecting_malformed_item() {
        let (base, vault, id, _manifest, digest) =
            staged_publish_fault_fixture("publish-idempotent-malformed");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        fs::write(
            base.join(format!(".trash/v1/items/{id}/manifest.json")),
            b"{",
        )
        .expect("malform published manifest");
        let mut attempts = Vec::new();

        let error = vault
            .trash_store()
            .publish_staging_item_with_hooks(
                id,
                &digest,
                |parent, _| {
                    attempts.push(parent);
                    match parent {
                        MoveSyncParent::Destination => Ok(MoveDurability::DirectorySyncUnsupported),
                        MoveSyncParent::Source => Ok(MoveDurability::FullySynced),
                    }
                },
                || {},
            )
            .expect_err("malformed published item");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Unsupported,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidTrashManifest(_))
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn publish_observes_topology_only_after_both_initial_sync_attempts() {
        let (base, vault, id, _manifest, digest) =
            staged_publish_fault_fixture("publish-observation-order");
        let staging = base.join(format!(".trash/v1/staging/{id}"));
        let items = base.join(format!(".trash/v1/items/{id}"));
        let mut attempts = Vec::new();

        let outcome = vault
            .trash_store()
            .publish_staging_item_with_hooks(
                id,
                &digest,
                |parent, _| {
                    attempts.push(parent);
                    if parent == MoveSyncParent::Source && staging.exists() {
                        fs::rename(&staging, &items).expect("external publication during sync");
                    }
                    Ok(MoveDurability::FullySynced)
                },
                || {},
            )
            .expect("idempotent publish after sync-time move");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert_eq!(
            outcome,
            PublishItemOutcome::AlreadyPublished(MoveDurability::FullySynced)
        );
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn publish_preobservation_error_preserves_initial_sync_statuses() {
        let (base, vault, id, _manifest, digest) =
            staged_publish_fault_fixture("publish-observation-error");
        let mut attempts = Vec::new();
        let error = vault
            .trash_store()
            .publish_staging_item_with_observation_hooks(
                id,
                &digest,
                |parent, _| {
                    attempts.push(parent);
                    match parent {
                        MoveSyncParent::Destination => Ok(MoveDurability::DirectorySyncUnsupported),
                        MoveSyncParent::Source => Ok(MoveDurability::FullySynced),
                    }
                },
                || Err(std::io::Error::other("injected lstat failure").into()),
                || {},
            )
            .expect_err("preobservation failure");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Unsupported,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::Io(_))
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn publish_both_and_neither_are_classified_after_initial_syncs() {
        for (label, create_items, remove_staging) in
            [("both", true, false), ("neither", false, true)]
        {
            let (base, vault, id, _manifest, digest) = staged_publish_fault_fixture(label);
            let staging = base.join(format!(".trash/v1/staging/{id}"));
            let items = base.join(format!(".trash/v1/items/{id}"));
            if create_items {
                fs::create_dir(&items).expect("items collision");
            }
            if remove_staging {
                fs::remove_dir_all(&staging).expect("remove staging");
            }
            let mut attempts = Vec::new();

            let error = vault
                .trash_store()
                .publish_staging_item_with_hooks(
                    id,
                    &digest,
                    |parent, _| {
                        attempts.push(parent);
                        Ok(MoveDurability::FullySynced)
                    },
                    || {},
                )
                .expect_err("invalid topology");

            assert_eq!(
                attempts,
                [MoveSyncParent::Destination, MoveSyncParent::Source]
            );
            assert!(matches!(error, CoreError::InvalidTrashTopology(_)));
            fs::remove_dir_all(base).expect("cleanup");
        }
    }

    #[test]
    fn publish_postrename_source_recreation_preserves_mixed_sync_statuses() {
        let (base, vault, id, _manifest, digest) =
            staged_publish_fault_fixture("publish-source-recreated");
        let staging = base.join(format!(".trash/v1/staging/{id}"));
        let error = vault
            .trash_store()
            .publish_staging_item_with_hooks(
                id,
                &digest,
                |parent, _| match parent {
                    MoveSyncParent::Destination => Ok(MoveDurability::FullySynced),
                    MoveSyncParent::Source => Ok(MoveDurability::DirectorySyncUnsupported),
                },
                || fs::create_dir(&staging).expect("recreate staging source"),
            )
            .expect_err("postrename source recreation");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Unsupported,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidTrashTopology(_))
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn publish_postrename_manifest_mutation_preserves_inverse_mixed_statuses() {
        let (base, vault, id, _manifest, digest) =
            staged_publish_fault_fixture("publish-manifest-mutated");
        let final_manifest = base.join(format!(".trash/v1/items/{id}/manifest.json"));
        let error = vault
            .trash_store()
            .publish_staging_item_with_hooks(
                id,
                &digest,
                |parent, _| match parent {
                    MoveSyncParent::Destination => Ok(MoveDurability::DirectorySyncUnsupported),
                    MoveSyncParent::Source => Ok(MoveDurability::FullySynced),
                },
                || fs::write(&final_manifest, b"{").expect("mutate item manifest"),
            )
            .expect_err("postrename manifest mutation");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Unsupported,
                source_sync: DirectorySyncStatus::Synced,
                ..
            }
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn publish_postrename_rejects_exact_looking_directory_swap() {
        let (base, vault, id, _manifest, digest) =
            staged_publish_fault_fixture("publish-exact-item-swap");
        let staging_manifest = fs::read(base.join(format!(".trash/v1/staging/{id}/manifest.json")))
            .expect("manifest bytes");
        let staging_payload =
            fs::read(base.join(format!(".trash/v1/staging/{id}/payload"))).expect("payload bytes");
        let items = base.join(format!(".trash/v1/items/{id}"));
        let detached = base.join(format!(".trash/v1/items/{id}.detached"));

        let error = vault
            .trash_store()
            .publish_staging_item_with_hooks(
                id,
                &digest,
                |_, _| Ok(MoveDurability::FullySynced),
                || {
                    fs::rename(&items, &detached).expect("detach published directory");
                    fs::create_dir(&items).expect("replacement directory");
                    fs::write(items.join("manifest.json"), &staging_manifest)
                        .expect("replacement manifest");
                    fs::write(items.join("payload"), &staging_payload)
                        .expect("replacement payload");
                },
            )
            .expect_err("published directory identity swap");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidTrashTopology(
                "trash item directory identity changed"
            ))
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_sync_failure_confirms_and_leaves_manifest() {
        let (base, vault, id, manifest, digest) = staged_publish_fault_fixture("restore-confirm");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        let mut attempts = Vec::new();
        let mut first_source = true;
        let outcome = vault
            .trash_store()
            .restore_item_with_hooks(
                id,
                &destination,
                &digest,
                |parent, _| {
                    attempts.push(parent);
                    if parent == MoveSyncParent::Source && first_source {
                        first_source = false;
                        Err(injected_sync_failure(parent))
                    } else {
                        Ok(MoveDurability::FullySynced)
                    }
                },
                || {},
            )
            .expect("confirmed restore");

        assert_eq!(
            attempts,
            [
                MoveSyncParent::Destination,
                MoveSyncParent::Source,
                MoveSyncParent::Destination,
                MoveSyncParent::Source,
            ]
        );
        assert_eq!(
            outcome,
            RestoreItemOutcome::Restored(MoveDurability::FullySynced)
        );
        assert!(base
            .join(format!(".trash/v1/items/{id}/manifest.json"))
            .is_file());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_idempotent_confirmation_syncs_before_rejecting_malformed_item() {
        let (base, vault, id, manifest, digest) =
            staged_publish_fault_fixture("restore-idempotent-malformed");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        vault
            .trash_store()
            .restore_item_if_revision(id, &destination, &digest)
            .expect("restore");
        fs::write(
            base.join(format!(".trash/v1/items/{id}/manifest.json")),
            b"{",
        )
        .expect("malform restored manifest");
        let mut attempts = Vec::new();

        let error = vault
            .trash_store()
            .restore_item_with_hooks(
                id,
                &destination,
                &digest,
                |parent, _| {
                    attempts.push(parent);
                    match parent {
                        MoveSyncParent::Destination => Ok(MoveDurability::FullySynced),
                        MoveSyncParent::Source => Ok(MoveDurability::DirectorySyncUnsupported),
                    }
                },
                || {},
            )
            .expect_err("malformed completed item");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Unsupported,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidTrashManifest(_))
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_observes_topology_only_after_both_initial_sync_attempts() {
        let (base, vault, id, manifest, digest) =
            staged_publish_fault_fixture("restore-observation-order");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        let payload = base.join(format!(".trash/v1/items/{id}/payload"));
        let destination_path = base.join(destination.as_path());
        let mut attempts = Vec::new();

        let outcome = vault
            .trash_store()
            .restore_item_with_hooks(
                id,
                &destination,
                &digest,
                |parent, _| {
                    attempts.push(parent);
                    if parent == MoveSyncParent::Source && payload.exists() {
                        fs::rename(&payload, &destination_path)
                            .expect("external restore during sync");
                    }
                    Ok(MoveDurability::FullySynced)
                },
                || {},
            )
            .expect("idempotent restore after sync-time move");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert_eq!(
            outcome,
            RestoreItemOutcome::AlreadyRestored(MoveDurability::FullySynced)
        );
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_preobservation_error_preserves_initial_sync_statuses() {
        let (base, vault, id, manifest, digest) =
            staged_publish_fault_fixture("restore-observation-error");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        let mut attempts = Vec::new();

        let error = vault
            .trash_store()
            .restore_item_with_observation_hooks(
                id,
                &destination,
                &digest,
                |parent, _| {
                    attempts.push(parent);
                    match parent {
                        MoveSyncParent::Destination => Ok(MoveDurability::FullySynced),
                        MoveSyncParent::Source => Ok(MoveDurability::DirectorySyncUnsupported),
                    }
                },
                || Err(std::io::Error::other("injected lstat failure").into()),
                || {},
            )
            .expect_err("preobservation failure");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Unsupported,
                verification,
                ..
            } if matches!(*verification, CoreError::Io(_))
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_both_and_neither_are_classified_after_initial_syncs() {
        for (label, create_destination, remove_payload) in
            [("both", true, false), ("neither", false, true)]
        {
            let (base, vault, id, manifest, digest) =
                staged_publish_fault_fixture(&format!("restore-{label}"));
            vault
                .trash_store()
                .publish_staging_item(id, &digest)
                .expect("publish");
            let destination =
                VaultPath::from_portable(&manifest.original_path).expect("destination");
            if create_destination {
                fs::write(base.join(destination.as_path()), b"external")
                    .expect("destination collision");
            }
            if remove_payload {
                fs::remove_file(base.join(format!(".trash/v1/items/{id}/payload")))
                    .expect("remove payload");
            }
            let mut attempts = Vec::new();

            let error = vault
                .trash_store()
                .restore_item_with_hooks(
                    id,
                    &destination,
                    &digest,
                    |parent, _| {
                        attempts.push(parent);
                        Ok(MoveDurability::FullySynced)
                    },
                    || {},
                )
                .expect_err("invalid restore topology");

            assert_eq!(
                attempts,
                [MoveSyncParent::Destination, MoveSyncParent::Source]
            );
            if create_destination {
                assert!(matches!(error, CoreError::AlreadyExists(_)));
            } else {
                assert!(matches!(error, CoreError::InvalidTrashTopology(_)));
            }
            fs::remove_dir_all(base).expect("cleanup");
        }
    }

    #[test]
    fn restore_postmove_manifest_mutation_is_explicit_unknown() {
        let (base, vault, id, manifest, digest) =
            staged_publish_fault_fixture("restore-manifest-mutated");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        let final_manifest = base.join(format!(".trash/v1/items/{id}/manifest.json"));
        let error = vault
            .trash_store()
            .restore_item_with_hooks(
                id,
                &destination,
                &digest,
                |parent, _| match parent {
                    MoveSyncParent::Destination => Ok(MoveDurability::FullySynced),
                    MoveSyncParent::Source => Ok(MoveDurability::DirectorySyncUnsupported),
                },
                || fs::write(&final_manifest, b"{").expect("mutate manifest"),
            )
            .expect_err("postmove manifest mutation");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Unsupported,
                ..
            }
        ));
        assert!(base.join("note.md").is_file());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_postmove_destination_mutation_is_explicit_unknown() {
        let (base, vault, id, manifest, digest) =
            staged_publish_fault_fixture("restore-destination-mutated");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        let destination_path = base.join(destination.as_path());
        let error = vault
            .trash_store()
            .restore_item_with_hooks(
                id,
                &destination,
                &digest,
                |parent, _| match parent {
                    MoveSyncParent::Destination => Ok(MoveDurability::DirectorySyncUnsupported),
                    MoveSyncParent::Source => Ok(MoveDurability::FullySynced),
                },
                || fs::write(&destination_path, b"external").expect("mutate destination"),
            )
            .expect_err("postmove destination mutation");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Unsupported,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::StaleRevision { .. })
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_postmove_payload_recreation_is_explicit_unknown() {
        let (base, vault, id, manifest, digest) =
            staged_publish_fault_fixture("restore-payload-recreated");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        let payload = base.join(format!(".trash/v1/items/{id}/payload"));
        let error = vault
            .trash_store()
            .restore_item_with_hooks(
                id,
                &destination,
                &digest,
                |parent, _| match parent {
                    MoveSyncParent::Destination => Ok(MoveDurability::FullySynced),
                    MoveSyncParent::Source => Ok(MoveDurability::DirectorySyncUnsupported),
                },
                || fs::write(&payload, b"note").expect("recreate payload"),
            )
            .expect_err("postmove payload recreation");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Unsupported,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidTrashTopology(_))
        ));
        assert!(base.join("note.md").is_file());
        assert!(payload.is_file());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_postmove_missing_authoritative_item_directory_is_explicit_unknown() {
        let (base, vault, id, manifest, digest) =
            staged_publish_fault_fixture("restore-item-directory-missing");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        let item = base.join(format!(".trash/v1/items/{id}"));
        let error = vault
            .trash_store()
            .restore_item_with_hooks(
                id,
                &destination,
                &digest,
                |_, _| Ok(MoveDurability::FullySynced),
                || {
                    fs::remove_file(item.join("manifest.json")).expect("remove manifest");
                    fs::remove_dir(&item).expect("remove item directory");
                },
            )
            .expect_err("missing authoritative item directory");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Synced,
                ..
            }
        ));
        assert!(base.join("note.md").is_file());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_confirmation_reports_source_not_attempted_when_reopen_fails() {
        let (base, vault, id, manifest, digest) =
            staged_publish_fault_fixture("restore-confirm-item-missing");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        let item = base.join(format!(".trash/v1/items/{id}"));
        let mut destination_syncs = 0_u8;
        let mut source_syncs = 0_u8;
        let error = vault
            .trash_store()
            .restore_item_with_hooks(
                id,
                &destination,
                &digest,
                |parent, _| match parent {
                    MoveSyncParent::Destination => {
                        destination_syncs = destination_syncs.saturating_add(1);
                        if destination_syncs == 3 {
                            Ok(MoveDurability::DirectorySyncUnsupported)
                        } else {
                            Ok(MoveDurability::FullySynced)
                        }
                    }
                    MoveSyncParent::Source => {
                        source_syncs = source_syncs.saturating_add(1);
                        if source_syncs == 2 {
                            Err(injected_sync_failure(parent))
                        } else {
                            Ok(MoveDurability::FullySynced)
                        }
                    }
                },
                || {
                    fs::remove_file(item.join("manifest.json")).expect("remove manifest");
                    fs::remove_dir(&item).expect("remove item directory");
                },
            )
            .expect_err("authoritative source reopen failure");

        assert_eq!(destination_syncs, 3);
        assert_eq!(source_syncs, 2);
        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Unsupported,
                source_sync: DirectorySyncStatus::NotAttempted,
                verification,
                ..
            } if matches!(*verification, CoreError::Io(ref error)
                if error.kind() == std::io::ErrorKind::NotFound)
        ));
        assert!(base.join("note.md").is_file());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn restore_postmove_rejects_malformed_and_exact_looking_directory_swaps() {
        for (label, exact_looking) in [("malformed", false), ("exact-looking", true)] {
            let (base, vault, id, manifest, digest) =
                staged_publish_fault_fixture(&format!("restore-item-swap-{label}"));
            vault
                .trash_store()
                .publish_staging_item(id, &digest)
                .expect("publish");
            let destination =
                VaultPath::from_portable(&manifest.original_path).expect("destination");
            let item = base.join(format!(".trash/v1/items/{id}"));
            let detached = base.join(format!(".trash/v1/items/{id}.detached"));
            let manifest_bytes = fs::read(item.join("manifest.json")).expect("manifest bytes");
            let error = vault
                .trash_store()
                .restore_item_with_hooks(
                    id,
                    &destination,
                    &digest,
                    |_, _| Ok(MoveDurability::FullySynced),
                    || {
                        fs::rename(&item, &detached).expect("detach item directory");
                        fs::create_dir(&item).expect("replacement item directory");
                        fs::write(
                            item.join("manifest.json"),
                            if exact_looking {
                                manifest_bytes.as_slice()
                            } else {
                                b"{".as_slice()
                            },
                        )
                        .expect("replacement manifest");
                    },
                )
                .expect_err("item directory identity swap");

            assert!(matches!(
                error,
                CoreError::VerifiedMoveOutcomeUnknown {
                    destination_sync: DirectorySyncStatus::Synced,
                    source_sync: DirectorySyncStatus::Synced,
                    verification,
                    ..
                } if matches!(*verification, CoreError::InvalidTrashTopology(
                    "trash item directory identity changed"
                ))
            ));
            fs::remove_dir_all(base).expect("cleanup");
        }
    }

    #[test]
    fn restore_confirmation_reopens_authoritative_item_directory() {
        let (base, vault, id, manifest, digest) =
            staged_publish_fault_fixture("restore-confirm-item-swap");
        vault
            .trash_store()
            .publish_staging_item(id, &digest)
            .expect("publish");
        let destination = VaultPath::from_portable(&manifest.original_path).expect("destination");
        let item = base.join(format!(".trash/v1/items/{id}"));
        let detached = base.join(format!(".trash/v1/items/{id}.detached"));
        let manifest_bytes = fs::read(item.join("manifest.json")).expect("manifest bytes");
        let mut source_syncs = 0_u8;
        let mut confirmation_synced_replacement = false;
        let error = vault
            .trash_store()
            .restore_item_with_hooks(
                id,
                &destination,
                &digest,
                |parent, directory| {
                    if parent == MoveSyncParent::Source {
                        source_syncs = source_syncs.saturating_add(1);
                        if source_syncs == 2 {
                            return Err(injected_sync_failure(parent));
                        }
                        if source_syncs == 3 {
                            let synced = directory.dir_metadata().expect("synced directory");
                            let path_bound = fs::metadata(&item).expect("path-bound directory");
                            confirmation_synced_replacement = synced.dev() == path_bound.dev()
                                && synced.ino() == path_bound.ino();
                        }
                    }
                    Ok(MoveDurability::FullySynced)
                },
                || {
                    fs::rename(&item, &detached).expect("detach item directory");
                    fs::create_dir(&item).expect("replacement item directory");
                    fs::write(item.join("manifest.json"), &manifest_bytes)
                        .expect("replacement manifest");
                },
            )
            .expect_err("confirmation must reject replacement inode");

        assert_eq!(source_syncs, 3);
        assert!(confirmation_synced_replacement);
        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidTrashTopology(
                "trash item directory identity changed"
            ))
        ));
        fs::remove_dir_all(base).expect("cleanup");
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

    #[test]
    fn content_move_prepublication_sync_failure_is_typed_and_does_not_move() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("content-pre-sync");
        let expected = FileRevision::from_bytes(b"note");
        let mut attempts = Vec::new();
        let error = vault
            .move_content_file_with_hooks(
                &source,
                &destination,
                &expected,
                |parent, _| {
                    attempts.push(parent);
                    if parent == MoveSyncParent::Destination {
                        Err(injected_sync_failure(parent))
                    } else {
                        Ok(MoveDurability::FullySynced)
                    }
                },
                || {},
                || {},
            )
            .expect_err("known prepublication failure");

        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Source]
        );
        assert!(matches!(
            error,
            CoreError::MoveContentPrepublicationSyncFailed {
                destination_sync: DirectorySyncStatus::Failed(_),
                source_sync: DirectorySyncStatus::Synced,
                ..
            }
        ));
        assert!(base.join(source.as_path()).is_file());
        assert!(!base.join(destination.as_path()).exists());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn content_move_propagates_unsupported_and_confirms_failed_publication_sync() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("content-confirm");
        let expected = FileRevision::from_bytes(b"note");
        let mut source_syncs = 0_u8;
        let outcome = vault
            .move_content_file_with_hooks(
                &source,
                &destination,
                &expected,
                |parent, _| {
                    if parent == MoveSyncParent::Source {
                        source_syncs = source_syncs.saturating_add(1);
                        if source_syncs == 2 {
                            return Err(injected_sync_failure(parent));
                        }
                    }
                    if parent == MoveSyncParent::Destination {
                        Ok(MoveDurability::DirectorySyncUnsupported)
                    } else {
                        Ok(MoveDurability::FullySynced)
                    }
                },
                || {},
                || {},
            )
            .expect("confirmed content move");

        assert_eq!(
            outcome,
            MoveContentOutcome::Moved(MoveDurability::DirectorySyncUnsupported)
        );
        let retry = vault
            .move_content_file_with_hooks(
                &source,
                &destination,
                &expected,
                |parent, _| {
                    if parent == MoveSyncParent::Destination {
                        Ok(MoveDurability::DirectorySyncUnsupported)
                    } else {
                        Ok(MoveDurability::FullySynced)
                    }
                },
                || {},
                || {},
            )
            .expect("retry");
        assert_eq!(
            retry,
            MoveContentOutcome::AlreadyMoved(MoveDurability::DirectorySyncUnsupported)
        );
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn content_move_combines_source_unsupported_symmetrically() {
        let (base, vault, source, destination) =
            atomic_move_sync_fixture("content-source-unsupported");
        let expected = FileRevision::from_bytes(b"note");
        let sync = |parent, _: &Dir| {
            if parent == MoveSyncParent::Source {
                Ok(MoveDurability::DirectorySyncUnsupported)
            } else {
                Ok(MoveDurability::FullySynced)
            }
        };

        let outcome = vault
            .move_content_file_with_hooks(&source, &destination, &expected, sync, || {}, || {})
            .expect("content move with unsupported source sync");
        assert_eq!(
            outcome,
            MoveContentOutcome::Moved(MoveDurability::DirectorySyncUnsupported)
        );

        let retry = vault
            .move_content_file_with_hooks(&source, &destination, &expected, sync, || {}, || {})
            .expect("idempotent retry with unsupported source sync");
        assert_eq!(
            retry,
            MoveContentOutcome::AlreadyMoved(MoveDurability::DirectorySyncUnsupported)
        );
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn content_move_postrename_destination_mutation_is_explicit_unknown() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("content-mutated");
        let expected = FileRevision::from_bytes(b"note");
        let destination_path = base.join(destination.as_path());
        let error = vault
            .move_content_file_with_hooks(
                &source,
                &destination,
                &expected,
                |_, _| Ok(MoveDurability::FullySynced),
                || {},
                || fs::write(&destination_path, b"external").expect("mutate destination"),
            )
            .expect_err("postrename mutation");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::StaleRevision { .. })
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn content_move_postrename_source_recreation_is_explicit_unknown() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("content-recreated");
        let expected = FileRevision::from_bytes(b"note");
        let source_path = base.join(source.as_path());
        let error = vault
            .move_content_file_with_hooks(
                &source,
                &destination,
                &expected,
                |parent, _| match parent {
                    MoveSyncParent::Destination => Ok(MoveDurability::DirectorySyncUnsupported),
                    MoveSyncParent::Source => Ok(MoveDurability::FullySynced),
                },
                || {},
                || fs::write(&source_path, b"external").expect("recreate source"),
            )
            .expect_err("postrename source recreation");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Unsupported,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidMove { .. })
        ));
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn content_move_rejects_parent_swap_immediately_before_rename() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("content-parent-swap");
        let expected = FileRevision::from_bytes(b"note");
        let source_parent = base.join("source-parent");
        let detached = base.join("source-parent-detached");
        let error = vault
            .move_content_file_with_hooks(
                &source,
                &destination,
                &expected,
                |_, _| Ok(MoveDurability::FullySynced),
                || {
                    fs::rename(&source_parent, &detached).expect("detach source parent");
                    fs::create_dir(&source_parent).expect("replacement source parent");
                },
                || {},
            )
            .expect_err("parent identity swap");

        assert!(matches!(error, CoreError::InvalidMove { .. }));
        assert!(detached.join("note.md").is_file());
        assert!(!base.join(destination.as_path()).exists());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn content_move_confirmation_rejects_parent_swap_with_exact_statuses() {
        let (base, vault, source, destination) = atomic_move_sync_fixture("content-confirm-swap");
        let expected = FileRevision::from_bytes(b"note");
        let destination_parent = base.join("destination-parent");
        let detached = base.join("destination-parent-detached");
        let mut source_syncs = 0_u8;
        let error = vault
            .move_content_file_with_hooks(
                &source,
                &destination,
                &expected,
                |parent, _| {
                    if parent == MoveSyncParent::Source {
                        source_syncs = source_syncs.saturating_add(1);
                        if source_syncs == 2 {
                            return Err(injected_sync_failure(parent));
                        }
                    }
                    Ok(MoveDurability::FullySynced)
                },
                || {},
                || {
                    fs::rename(&destination_parent, &detached).expect("detach destination parent");
                    fs::create_dir(&destination_parent).expect("replacement destination parent");
                },
            )
            .expect_err("confirmation parent identity swap");

        assert!(matches!(
            error,
            CoreError::VerifiedMoveOutcomeUnknown {
                destination_sync: DirectorySyncStatus::Synced,
                source_sync: DirectorySyncStatus::Synced,
                verification,
                ..
            } if matches!(*verification, CoreError::InvalidMove { .. })
        ));
        assert!(detached.join("note.md").is_file());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn content_move_same_parent_syncs_once_per_phase() {
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("temp root");
        let base = temp_root.join(format!(
            "myvault-content-same-parent-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).expect("fixture");
        fs::write(base.join("from.md"), b"note").expect("source");
        let vault = Vault::open(&base).expect("vault");
        let source = VaultPath::from_portable("from.md").expect("source path");
        let destination = VaultPath::from_portable("to.md").expect("destination path");
        let mut attempts = Vec::new();
        let outcome = vault
            .move_content_file_with_hooks(
                &source,
                &destination,
                &FileRevision::from_bytes(b"note"),
                |parent, _| {
                    attempts.push(parent);
                    Ok(MoveDurability::FullySynced)
                },
                || {},
                || {},
            )
            .expect("move");

        assert_eq!(
            outcome,
            MoveContentOutcome::Moved(MoveDurability::FullySynced)
        );
        assert_eq!(
            attempts,
            [MoveSyncParent::Destination, MoveSyncParent::Destination]
        );
        fs::remove_dir_all(base).expect("cleanup");
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
        let initial = vault
            .atomic_move_locked_report(&source, &destination, |parent, _| {
                if parent == MoveSyncParent::Destination {
                    Err(injected_sync_failure(parent))
                } else {
                    Ok(MoveDurability::FullySynced)
                }
            })
            .and_then(|report| report.into_result(&source, &destination));
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
        let initial = vault
            .atomic_move_locked_report(&source, &destination, |parent, _| {
                if parent == MoveSyncParent::Source {
                    Err(injected_sync_failure(parent))
                } else {
                    Ok(MoveDurability::FullySynced)
                }
            })
            .and_then(|report| report.into_result(&source, &destination));
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
    fn final_verification_preserves_exact_mixed_parent_sync_statuses() {
        for (label, destination_result, source_result) in [
            (
                "destination-synced",
                MoveDurability::FullySynced,
                MoveDurability::DirectorySyncUnsupported,
            ),
            (
                "source-synced",
                MoveDurability::DirectorySyncUnsupported,
                MoveDurability::FullySynced,
            ),
        ] {
            let (base, vault, source, destination) = atomic_move_sync_fixture(label);
            let guard = vault.lock_mutations().expect("lock");
            let report = vault
                .atomic_move_locked_report(&source, &destination, |parent, _| match parent {
                    MoveSyncParent::Destination => Ok(destination_result),
                    MoveSyncParent::Source => Ok(source_result),
                })
                .expect("published move report");
            fs::write(base.join(destination.as_path()), b"external")
                .expect("mutate destination after sync");

            let error = vault
                .finish_verified_move(
                    &source,
                    &destination,
                    &FileRevision::from_bytes(b"note"),
                    64,
                    report,
                )
                .expect_err("final verification must be outcome unknown");

            let expected_destination = match destination_result {
                MoveDurability::FullySynced => DirectorySyncStatus::Synced,
                MoveDurability::DirectorySyncUnsupported => DirectorySyncStatus::Unsupported,
            };
            let expected_source = match source_result {
                MoveDurability::FullySynced => DirectorySyncStatus::Synced,
                MoveDurability::DirectorySyncUnsupported => DirectorySyncStatus::Unsupported,
            };
            assert!(matches!(
                error,
                CoreError::VerifiedMoveOutcomeUnknown {
                    destination_sync,
                    source_sync,
                    verification,
                    ..
                } if std::mem::discriminant(&destination_sync)
                    == std::mem::discriminant(&expected_destination)
                    && std::mem::discriminant(&source_sync)
                        == std::mem::discriminant(&expected_source)
                    && matches!(*verification, CoreError::StaleRevision { .. })
            ));
            drop(guard);
            fs::remove_dir_all(base).expect("cleanup");
        }
    }

    #[test]
    fn confirm_sync_failure_after_valid_topology_is_verified_outcome_unknown() {
        for (label, fail_source_too) in [("one-sync-fails", false), ("both-sync-fail", true)] {
            let (base, vault, source, destination) = atomic_move_sync_fixture(label);
            fs::rename(
                base.join(source.as_path()),
                base.join(destination.as_path()),
            )
            .expect("published topology");
            let guard = vault.lock_mutations().expect("lock");
            let mut attempts = Vec::new();

            let error = vault
                .confirm_move_durability_with_sync(
                    &source,
                    &destination,
                    &FileRevision::from_bytes(b"note"),
                    64,
                    |parent, _| {
                        attempts.push(parent);
                        if parent == MoveSyncParent::Destination
                            || (parent == MoveSyncParent::Source && fail_source_too)
                        {
                            Err(injected_sync_failure(parent))
                        } else {
                            Ok(MoveDurability::FullySynced)
                        }
                    },
                )
                .expect_err("sync-only failure remains known-published outcome unknown");

            assert_eq!(
                attempts,
                [MoveSyncParent::Destination, MoveSyncParent::Source]
            );
            assert!(matches!(
                error,
                CoreError::VerifiedMoveOutcomeUnknown {
                    destination_sync: DirectorySyncStatus::Failed(_),
                    source_sync,
                    verification,
                    ..
                } if (fail_source_too
                    && matches!(source_sync, DirectorySyncStatus::Failed(_))
                    || !fail_source_too
                        && matches!(source_sync, DirectorySyncStatus::Synced))
                    && matches!(*verification, CoreError::MoveDurabilitySyncFailed)
            ));
            drop(guard);
            fs::remove_dir_all(base).expect("cleanup");
        }
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
