#![forbid(unsafe_code)]

//! Immutable, bounded recovery snapshots for note-content replacement.
//!
//! Publication is append-only. Failed work directories and stable evidence
//! are never removed or repaired by this crate.

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use myvault_core::VaultPath;
use myvault_private_fs as private_fs;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use uuid::Uuid;

mod quarantine;
mod retention;
pub use quarantine::{DeletionOutcome, DeletionReport};
pub use quarantine::{GcCandidate, GcPlan, QuarantineOutcome, QuarantineReport};
pub use retention::{
    RetentionCandidate, RetentionPlan, RetentionPolicy, RetentionReason, MAX_RETENTION_CANDIDATES,
    MAX_SNAPSHOT_SCAN_ENTRIES, MAX_VERIFICATION_BYTES,
};

const ROOT_DIRECTORY: &str = "recovery-snapshots";
const VERSION_DIRECTORY: &str = "v1";
const VAULTS_DIRECTORY: &str = "vaults";
const BINDING_FILE: &str = "binding.json";
const STAGING_DIRECTORY: &str = "staging";
const OBJECTS_DIRECTORY: &str = "objects";
const MANIFEST_FILE: &str = "manifest.json";
const PAYLOAD_FILE: &str = "payload";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const OPERATION_LOCK_FILE: &str = "operation.lock";

pub const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
pub const MAX_PAYLOAD_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurabilityBoundary {
    BindingDirectory,
    WorkDirectory,
    StagingDirectory,
    ObjectsDirectory,
    QuarantineRun,
    QuarantineRuns,
    QuarantineWork,
    QuarantineItems,
    QuarantineState,
    QuarantineMarkerStaging,
    SourceObjects,
    DeletionItem,
    DeletionItems,
    DeletionRun,
    DeletionRuns,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidRoot(&'static str),
    PrivacyValidationRequired,
    ExtendedAcl,
    ExternalMutation,
    InvalidVaultId,
    InvalidSnapshotId,
    InvalidRevision,
    InvalidNotePath,
    ManifestTooLarge,
    PayloadTooLarge,
    UnsupportedVersion(u32),
    InvalidObjectTopology,
    SnapshotNotFound,
    SnapshotCollision,
    BindingCollision,
    AmbiguousEvidence,
    TooManySnapshotEntries,
    ArithmeticOverflow,
    VerificationBudgetExceeded,
    OperationLockLost,
    PublishedButLockLost(PublishOutcome),
    OperationFailedAndLockLost(Box<Error>),
    GcPlanTooLarge,
    TooManyGcRuns,
    InvalidGcPlan,
    QuarantineCollision,
    QuarantinedButLockLost(QuarantineReport),
    DeletedButLockLost(DeletionReport),
    DetachedButNotSynced {
        run_id: Uuid,
        snapshot_id: Uuid,
        boundary: DurabilityBoundary,
        source: Box<Error>,
    },
    DetachedOutcomeUnknown {
        run_id: Uuid,
        snapshot_id: Uuid,
        source: Box<Error>,
    },
    RemovedButNotSynced {
        run_id: Uuid,
        snapshot_id: Option<Uuid>,
        boundary: DurabilityBoundary,
        source: Box<Error>,
    },
    RemovedAndSyncedButInterrupted {
        run_id: Uuid,
        snapshot_id: Option<Uuid>,
        boundary: DurabilityBoundary,
        source: Box<Error>,
    },
    PublishedButNotSynced {
        boundary: DurabilityBoundary,
        source: private_fs::Error,
    },
}

impl fmt::Display for Error {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "invalid snapshot JSON: {error}"),
            Self::InvalidRoot(reason) => write!(formatter, "invalid snapshot root: {reason}"),
            Self::PrivacyValidationRequired => formatter
                .write_str("snapshot store disabled: exact Unix privacy validation is required"),
            Self::ExtendedAcl => formatter.write_str("snapshot object has an extended ACL"),
            Self::ExternalMutation => {
                formatter.write_str("snapshot topology was modified externally")
            }
            Self::InvalidVaultId => formatter.write_str("invalid vault id"),
            Self::InvalidSnapshotId => formatter.write_str("invalid snapshot id"),
            Self::InvalidRevision => formatter.write_str("invalid snapshot revision"),
            Self::InvalidNotePath => formatter.write_str("snapshot path must be a canonical note"),
            Self::ManifestTooLarge => formatter.write_str("snapshot manifest exceeds 16 KiB"),
            Self::PayloadTooLarge => formatter.write_str("snapshot payload exceeds 16 MiB"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported snapshot version {version}")
            }
            Self::InvalidObjectTopology => formatter
                .write_str("snapshot object must contain exactly manifest.json and payload"),
            Self::SnapshotNotFound => formatter.write_str("snapshot evidence was not found"),
            Self::SnapshotCollision => {
                formatter.write_str("snapshot id is bound to different evidence")
            }
            Self::BindingCollision => {
                formatter.write_str("vault id is bound to a different vault root")
            }
            Self::AmbiguousEvidence => {
                formatter.write_str("snapshot exists in both staging and objects")
            }
            Self::TooManySnapshotEntries => {
                formatter.write_str("snapshot inventory exceeds 8192 physical entries")
            }
            Self::ArithmeticOverflow => formatter.write_str("snapshot byte arithmetic overflow"),
            Self::VerificationBudgetExceeded => {
                formatter.write_str("snapshot verification budget exceeded")
            }
            Self::OperationLockLost => {
                formatter.write_str("operation lock identity was lost before completion")
            }
            Self::PublishedButLockLost(outcome) => write!(
                formatter,
                "snapshot publication reached {outcome:?} but operation lock was lost"
            ),
            Self::OperationFailedAndLockLost(error) => write!(
                formatter,
                "operation failed and its lock was also lost: {error}"
            ),
            Self::GcPlanTooLarge => formatter.write_str("GC plan exceeds 128 KiB"),
            Self::TooManyGcRuns => formatter.write_str("quarantine contains more than 128 runs"),
            Self::InvalidGcPlan => formatter.write_str("invalid or opaque GC plan evidence"),
            Self::QuarantineCollision => formatter.write_str("quarantine topology mismatch"),
            Self::QuarantinedButLockLost(report) => write!(
                formatter,
                "quarantine run {} detached {} items but operation lock was lost",
                report.run_id, report.detached
            ),
            Self::DeletedButLockLost(report) => write!(
                formatter,
                "quarantine run {} deletion completed but operation lock was lost",
                report.run_id
            ),
            Self::DetachedButNotSynced {
                run_id,
                snapshot_id,
                boundary,
                source,
            } => write!(
                formatter,
                "snapshot {snapshot_id} detached in run {run_id} but {boundary:?} sync failed: {source}"
            ),
            Self::DetachedOutcomeUnknown {
                run_id,
                snapshot_id,
                source,
            } => write!(
                formatter,
                "snapshot {snapshot_id} detach outcome in run {run_id} is unknown: {source}"
            ),
            Self::RemovedButNotSynced {
                run_id,
                snapshot_id,
                boundary,
                source,
            } => write!(
                formatter,
                "quarantine evidence {snapshot_id:?} removed in run {run_id} but {boundary:?} sync failed: {source}"
            ),
            Self::RemovedAndSyncedButInterrupted {
                run_id,
                snapshot_id,
                boundary,
                source,
            } => write!(
                formatter,
                "quarantine evidence {snapshot_id:?} was removed and {boundary:?} synced in run {run_id}, then processing was interrupted: {source}"
            ),
            Self::PublishedButNotSynced { boundary, source } => {
                write!(
                    formatter,
                    "snapshot published at {boundary:?} but sync failed: {source}"
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::PublishedButNotSynced { source, .. } => Some(source),
            _ => None,
        }
    }
}

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

impl From<private_fs::Error> for Error {
    fn from(value: private_fs::Error) -> Self {
        match value {
            private_fs::Error::Io(error) | private_fs::Error::DirectorySyncUnsupported(error) => {
                Self::Io(error)
            }
            private_fs::Error::InvalidRoot(reason) => Self::InvalidRoot(reason),
            private_fs::Error::PrivacyValidationRequired => Self::PrivacyValidationRequired,
            private_fs::Error::ExtendedAcl => Self::ExtendedAcl,
            private_fs::Error::ExternalMutation => Self::ExternalMutation,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRevision {
    pub blake3_hex: String,
    pub byte_len: u64,
}

impl SnapshotRevision {
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            blake3_hex: blake3::hash(bytes).to_hex().to_string(),
            byte_len: bytes.len() as u64,
        }
    }

    fn validate(&self) -> Result<(), Error> {
        if self.byte_len > MAX_PAYLOAD_BYTES
            || self.blake3_hex.len() != 64
            || !self
                .blake3_hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::InvalidRevision);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotReason {
    BeforeContentReplace,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    pub version: u32,
    #[serde(deserialize_with = "deserialize_canonical_nonnil_uuid")]
    pub snapshot_id: Uuid,
    #[serde(deserialize_with = "deserialize_canonical_nonnil_uuid")]
    pub vault_id: Uuid,
    pub path: String,
    pub created_at_unix_ms: u64,
    pub revision: SnapshotRevision,
    pub reason: SnapshotReason,
}

impl SnapshotManifest {
    pub const VERSION: u32 = 1;

    /// Creates a canonical v1 note snapshot manifest.
    ///
    /// # Errors
    /// Rejects nil identifiers, noncanonical/non-note paths, and invalid revisions.
    pub fn new(
        snapshot_id: Uuid,
        vault_id: Uuid,
        path: impl AsRef<str>,
        created_at_unix_ms: u64,
        revision: SnapshotRevision,
    ) -> Result<Self, Error> {
        let manifest = Self {
            version: Self::VERSION,
            snapshot_id,
            vault_id,
            path: path.as_ref().to_owned(),
            created_at_unix_ms,
            revision,
            reason: SnapshotReason::BeforeContentReplace,
        };
        manifest.validate()?;
        canonical_manifest_bytes(&manifest)?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), Error> {
        if self.version != Self::VERSION {
            return Err(Error::UnsupportedVersion(self.version));
        }
        validate_ids(self.snapshot_id, self.vault_id)?;
        self.revision.validate()?;
        let path = VaultPath::from_portable(&self.path).map_err(|_| Error::InvalidNotePath)?;
        let is_supported_note = Path::new(&self.path)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "md" | "MD"));
        if path.as_str() != self.path
            || !is_supported_note
            || matches!(
                path.collision_key().split('/').next(),
                Some(".trash" | ".obsidian")
            )
        {
            return Err(Error::InvalidNotePath);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceLocation {
    Staging,
    Objects,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotEvidence {
    Supported {
        location: EvidenceLocation,
        manifest: SnapshotManifest,
    },
    Unsupported {
        location: EvidenceLocation,
        snapshot_id: Uuid,
        vault_id: Uuid,
        version: u32,
        payload_revision: SnapshotRevision,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Published,
    AlreadyPublished,
    PromotedFromStaging,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultBinding {
    version: u32,
    #[serde(deserialize_with = "deserialize_canonical_nonnil_uuid")]
    vault_id: Uuid,
    unix_device: u64,
    unix_inode: u64,
}

impl VaultBinding {
    const VERSION: u32 = 1;
}

pub struct SnapshotStore {
    _roots: private_fs::PrivateDisjointRoots,
    vault_id: Uuid,
    staging: Dir,
    objects: Dir,
    vault: Dir,
    lock_identity: LockIdentity,
}

impl SnapshotStore {
    /// Opens a vault-bound private snapshot store.
    ///
    /// # Errors
    /// Fails closed on unsupported targets, insecure roots, or a binding mismatch.
    pub fn open(app_data_root: &Path, vault_root: &Path, vault_id: Uuid) -> Result<Self, Error> {
        if vault_id.is_nil() {
            return Err(Error::InvalidVaultId);
        }
        let roots =
            private_fs::open_private_disjoint_roots_with_unix_identity(app_data_root, vault_root)?;
        let recovery =
            private_fs::create_or_open_private_dir(roots.private_root(), ROOT_DIRECTORY)?;
        let version = private_fs::create_or_open_private_dir(&recovery, VERSION_DIRECTORY)?;
        let vaults = private_fs::create_or_open_private_dir(&version, VAULTS_DIRECTORY)?;
        let vault = private_fs::create_or_open_private_dir(&vaults, vault_id.to_string())?;
        let identity = roots.other_identity();
        let binding = VaultBinding {
            version: VaultBinding::VERSION,
            vault_id,
            unix_device: identity.device(),
            unix_inode: identity.inode(),
        };
        publish_or_verify_binding(&vault, &binding)?;
        let lock_identity = ensure_operation_lock(&vault)?;
        let staging = private_fs::create_or_open_private_dir(&vault, STAGING_DIRECTORY)?;
        let objects = private_fs::create_or_open_private_dir(&vault, OBJECTS_DIRECTORY)?;
        Ok(Self {
            _roots: roots,
            vault_id,
            staging,
            objects,
            vault,
            lock_identity,
        })
    }

    /// Publishes one immutable snapshot or safely resumes an exact stable retry.
    ///
    /// # Errors
    /// Fails closed on mismatches, ambiguous evidence, topology changes, or durability failure.
    pub fn publish(
        &self,
        manifest: &SnapshotManifest,
        payload: &[u8],
    ) -> Result<PublishOutcome, Error> {
        let operation = self.lock_operation()?;
        let result = (|| {
            manifest.validate()?;
            if manifest.vault_id != self.vault_id {
                return Err(Error::InvalidVaultId);
            }
            if payload.len() as u64 > MAX_PAYLOAD_BYTES {
                return Err(Error::PayloadTooLarge);
            }
            if SnapshotRevision::from_bytes(payload) != manifest.revision {
                return Err(Error::InvalidRevision);
            }
            let manifest_bytes = canonical_manifest_bytes(manifest)?;
            if let Some(outcome) = self.resume_stable(manifest, &manifest_bytes)? {
                return Ok(outcome);
            }

            let (work_name, work) = self.create_fresh_work(manifest.snapshot_id)?;
            write_private_file(&work, PAYLOAD_FILE, payload)?;
            write_private_file(&work, MANIFEST_FILE, &manifest_bytes)?;
            verify_expected_object(&work, manifest, &manifest_bytes)?;
            sync_published(&work, DurabilityBoundary::WorkDirectory)?;
            drop(work);

            let stable_name = manifest.snapshot_id.to_string();
            match atomic_rename_noreplace(&self.staging, &work_name, &self.staging, &stable_name) {
                Ok(()) => sync_published(&self.staging, DurabilityBoundary::StagingDirectory)?,
                Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return self
                        .resume_stable(manifest, &manifest_bytes)?
                        .ok_or(Error::SnapshotCollision);
                }
                Err(error) => return Err(error),
            }
            self.promote_staging(manifest, &manifest_bytes, PublishOutcome::Published)
        })();
        finish_publish_operation(operation, result)
    }

    /// Inspects one immutable stable object without interpreting future schemas.
    ///
    /// # Errors
    /// Rejects absent, ambiguous, malformed, insecure, or mismatched evidence.
    pub fn inspect(&self, snapshot_id: Uuid) -> Result<SnapshotEvidence, Error> {
        if snapshot_id.is_nil() {
            return Err(Error::InvalidSnapshotId);
        }
        let name = snapshot_id.to_string();
        let object = open_optional_private_dir(&self.objects, &name)?;
        let staging = open_optional_private_dir(&self.staging, &name)?;
        match (object, staging) {
            (Some(_), Some(_)) => Err(Error::AmbiguousEvidence),
            (Some(directory), None) => inspect_object(
                &directory,
                EvidenceLocation::Objects,
                snapshot_id,
                self.vault_id,
                None,
            )
            .and_then(|(evidence, _)| evidence),
            (None, Some(directory)) => inspect_object(
                &directory,
                EvidenceLocation::Staging,
                snapshot_id,
                self.vault_id,
                None,
            )
            .and_then(|(evidence, _)| evidence),
            (None, None) => Err(Error::SnapshotNotFound),
        }
    }

    fn create_fresh_work(&self, snapshot_id: Uuid) -> Result<(String, Dir), Error> {
        for _ in 0..32 {
            let name = format!(".work-{snapshot_id}-{}", Uuid::new_v4());
            match private_fs::create_private_dir(&self.staging, &name) {
                Ok(work) => return Ok((name, work)),
                Err(private_fs::Error::Io(error))
                    if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::SnapshotCollision)
    }

    fn lock_operation(&self) -> Result<OperationGuard, Error> {
        acquire_operation_lock(&self.vault, self.lock_identity)
    }

    fn resume_stable(
        &self,
        manifest: &SnapshotManifest,
        manifest_bytes: &[u8],
    ) -> Result<Option<PublishOutcome>, Error> {
        let name = manifest.snapshot_id.to_string();
        let object = open_optional_private_dir(&self.objects, &name)?;
        let staging = open_optional_private_dir(&self.staging, &name)?;
        match (object, staging) {
            (Some(_), Some(_)) => Err(Error::AmbiguousEvidence),
            (Some(directory), None) => {
                verify_expected_object(&directory, manifest, manifest_bytes)?;
                sync_published(&self.objects, DurabilityBoundary::ObjectsDirectory)?;
                sync_published(&self.staging, DurabilityBoundary::StagingDirectory)?;
                Ok(Some(PublishOutcome::AlreadyPublished))
            }
            (None, Some(directory)) => {
                verify_expected_object(&directory, manifest, manifest_bytes)?;
                drop(directory);
                self.promote_staging(
                    manifest,
                    manifest_bytes,
                    PublishOutcome::PromotedFromStaging,
                )
                .map(Some)
            }
            (None, None) => Ok(None),
        }
    }

    fn promote_staging(
        &self,
        manifest: &SnapshotManifest,
        manifest_bytes: &[u8],
        outcome: PublishOutcome,
    ) -> Result<PublishOutcome, Error> {
        let name = manifest.snapshot_id.to_string();
        match atomic_rename_noreplace(&self.staging, &name, &self.objects, &name) {
            Ok(()) => {}
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Error::AmbiguousEvidence);
            }
            Err(error) => return Err(error),
        }
        sync_published(&self.objects, DurabilityBoundary::ObjectsDirectory)?;
        sync_published(&self.staging, DurabilityBoundary::StagingDirectory)?;
        let object = private_fs::open_private_dir(&self.objects, &name)?;
        verify_expected_object(&object, manifest, manifest_bytes)?;
        Ok(outcome)
    }
}

struct OperationGuard {
    file: cap_std::fs::File,
    vault: Dir,
    expected_identity: LockIdentity,
}

impl OperationGuard {
    fn finish(self) -> Result<(), Error> {
        validate_locked_file(&self.file, &self.vault, self.expected_identity)
    }
}

#[derive(Clone, Copy)]
struct LockIdentity {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    device: u64,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    inode: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ensure_operation_lock(vault: &Dir) -> Result<LockIdentity, Error> {
    use cap_fs_ext::MetadataExt;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    match vault.open_with(OPERATION_LOCK_FILE, &options) {
        Ok(file) => {
            private_fs::set_private_file_permissions(&file)?;
            file.sync_all()?;
            private_fs::verify_private_file(&file, 1)?;
            sync_published(vault, DurabilityBoundary::BindingDirectory)?;
            let metadata = file.metadata()?;
            Ok(LockIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut existing = OpenOptions::new();
            existing.read(true).write(true).follow(FollowSymlinks::No);
            let file = vault.open_with(OPERATION_LOCK_FILE, &existing)?;
            private_fs::verify_private_file(&file, 1)?;
            let metadata = file.metadata()?;
            Ok(LockIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn ensure_operation_lock(_vault: &Dir) -> Result<LockIdentity, Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn acquire_operation_lock(
    vault: &Dir,
    expected_identity: LockIdentity,
) -> Result<OperationGuard, Error> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    let file = vault.open_with(OPERATION_LOCK_FILE, &options)?;
    private_fs::verify_private_file(&file, 1)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).map_err(|error| {
        let error = io::Error::from(error);
        if error.kind() == io::ErrorKind::Unsupported {
            Error::PrivacyValidationRequired
        } else {
            Error::Io(error)
        }
    })?;
    validate_locked_file(&file, vault, expected_identity)?;
    Ok(OperationGuard {
        file,
        vault: vault.try_clone()?,
        expected_identity,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn acquire_operation_lock(
    _vault: &Dir,
    _expected_identity: LockIdentity,
) -> Result<OperationGuard, Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn validate_locked_file(
    file: &cap_std::fs::File,
    vault: &Dir,
    expected_identity: LockIdentity,
) -> Result<(), Error> {
    use cap_fs_ext::MetadataExt;

    private_fs::verify_private_file(file, 1)?;
    let held = file.metadata()?;
    let named = vault.symlink_metadata(OPERATION_LOCK_FILE)?;
    if !named.file_type().is_file()
        || held.dev() != named.dev()
        || held.ino() != named.ino()
        || held.dev() != expected_identity.device
        || held.ino() != expected_identity.inode
        || held.nlink() != 1
    {
        return Err(Error::ExternalMutation);
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn validate_locked_file(
    _file: &cap_std::fs::File,
    _vault: &Dir,
    _expected_identity: LockIdentity,
) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

fn finish_publish_operation(
    operation: OperationGuard,
    result: Result<PublishOutcome, Error>,
) -> Result<PublishOutcome, Error> {
    match (result, operation.finish()) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(outcome), Err(_)) => Err(Error::PublishedButLockLost(outcome)),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(_)) => Err(Error::OperationFailedAndLockLost(Box::new(error))),
    }
}

fn publish_or_verify_binding(directory: &Dir, binding: &VaultBinding) -> Result<(), Error> {
    let bytes = serde_json::to_vec(binding)?;
    if let Some(actual) = read_optional_private_file(directory, BINDING_FILE, MAX_MANIFEST_BYTES)? {
        let observed: VaultBinding = serde_json::from_slice(&actual)?;
        if observed != *binding || serde_json::to_vec(&observed)? != actual {
            return Err(Error::BindingCollision);
        }
        return sync_published(directory, DurabilityBoundary::BindingDirectory);
    }
    let temporary = format!(".binding-{}.tmp", Uuid::new_v4());
    write_private_file(directory, &temporary, &bytes)?;
    match atomic_rename_noreplace(directory, &temporary, directory, BINDING_FILE) {
        Ok(()) => sync_published(directory, DurabilityBoundary::BindingDirectory),
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            let actual = read_private_file(directory, BINDING_FILE, MAX_MANIFEST_BYTES)?;
            if actual == bytes {
                sync_published(directory, DurabilityBoundary::BindingDirectory)
            } else {
                Err(Error::BindingCollision)
            }
        }
        Err(error) => Err(error),
    }
}

fn write_private_file(directory: &Dir, name: &str, bytes: &[u8]) -> Result<(), Error> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = directory.open_with(name, &options)?;
    private_fs::set_private_file_permissions(&file)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    private_fs::verify_private_file(&file, 1)?;
    Ok(())
}

fn verify_expected_object(
    directory: &Dir,
    expected: &SnapshotManifest,
    expected_bytes: &[u8],
) -> Result<(), Error> {
    verify_exact_object_entries(directory)?;
    let bytes = read_private_file(directory, MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
    if bytes != expected_bytes {
        return Err(Error::SnapshotCollision);
    }
    let observed: SnapshotManifest = serde_json::from_slice(&bytes)?;
    observed.validate()?;
    if observed != *expected || canonical_manifest_bytes(&observed)? != bytes {
        return Err(Error::SnapshotCollision);
    }
    let revision = read_payload_revision(directory)?;
    if revision != expected.revision {
        return Err(Error::SnapshotCollision);
    }
    Ok(())
}

fn inspect_object(
    directory: &Dir,
    location: EvidenceLocation,
    expected_snapshot_id: Uuid,
    expected_vault_id: Uuid,
    verification_budget: Option<&mut u64>,
) -> Result<(Result<SnapshotEvidence, Error>, u64), Error> {
    verify_exact_object_entries(directory)?;
    let (manifest_file, manifest_len) =
        open_held_private_file(directory, MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
    let (payload_file, payload_len) =
        open_held_private_file(directory, PAYLOAD_FILE, MAX_PAYLOAD_BYTES)?;
    let logical_bytes = manifest_len
        .checked_add(payload_len)
        .ok_or(Error::ArithmeticOverflow)?;
    if let Some(budget) = verification_budget {
        if logical_bytes > *budget {
            return Err(Error::VerificationBudgetExceeded);
        }
        *budget = budget
            .checked_sub(logical_bytes)
            .ok_or(Error::ArithmeticOverflow)?;
    }
    let bytes = read_held_private_file(manifest_file, manifest_len)?;
    let payload = read_held_private_file(payload_file, payload_len)?;
    let payload_revision = SnapshotRevision::from_bytes(&payload);
    let evidence = (|| {
        let envelope: RoutingEnvelope = serde_json::from_slice(&bytes)?;
        if envelope.snapshot_id != expected_snapshot_id || envelope.vault_id != expected_vault_id {
            return Err(Error::SnapshotCollision);
        }
        if envelope.version != SnapshotManifest::VERSION {
            return Ok(SnapshotEvidence::Unsupported {
                location,
                snapshot_id: envelope.snapshot_id,
                vault_id: envelope.vault_id,
                version: envelope.version,
                payload_revision,
            });
        }
        let manifest: SnapshotManifest = serde_json::from_slice(&bytes)?;
        manifest.validate()?;
        if canonical_manifest_bytes(&manifest)? != bytes || payload_revision != manifest.revision {
            return Err(Error::SnapshotCollision);
        }
        Ok(SnapshotEvidence::Supported { location, manifest })
    })();
    Ok((evidence, logical_bytes))
}

fn verify_exact_object_entries(directory: &Dir) -> Result<(), Error> {
    let mut manifest = false;
    let mut payload = false;
    let mut count = 0_u8;
    for entry in directory.entries()? {
        let entry = entry?;
        count = count.checked_add(1).ok_or(Error::InvalidObjectTopology)?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            return Err(Error::InvalidObjectTopology);
        };
        match name {
            MANIFEST_FILE if !manifest => manifest = true,
            PAYLOAD_FILE if !payload => payload = true,
            _ => return Err(Error::InvalidObjectTopology),
        }
    }
    if count != 2 || !manifest || !payload {
        return Err(Error::InvalidObjectTopology);
    }
    Ok(())
}

fn read_payload_revision(directory: &Dir) -> Result<SnapshotRevision, Error> {
    let bytes = read_private_file(directory, PAYLOAD_FILE, MAX_PAYLOAD_BYTES)?;
    Ok(SnapshotRevision::from_bytes(&bytes))
}

fn read_optional_private_file(
    directory: &Dir,
    name: &str,
    maximum: u64,
) -> Result<Option<Vec<u8>>, Error> {
    match read_private_file(directory, name, maximum) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_private_file(directory: &Dir, name: &str, maximum: u64) -> Result<Vec<u8>, Error> {
    let (file, length) = open_held_private_file(directory, name, maximum)?;
    read_held_private_file(file, length)
}

fn open_held_private_file(
    directory: &Dir,
    name: &str,
    maximum: u64,
) -> Result<(cap_std::fs::File, u64), Error> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options)?;
    private_fs::verify_private_file(&file, 1)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::ExternalMutation);
    }
    if metadata.len() > maximum {
        return Err(if name == PAYLOAD_FILE {
            Error::PayloadTooLarge
        } else {
            Error::ManifestTooLarge
        });
    }
    Ok((file, metadata.len()))
}

fn read_held_private_file(
    mut file: cap_std::fs::File,
    expected_length: u64,
) -> Result<Vec<u8>, Error> {
    let capacity = usize::try_from(expected_length).map_err(|_| Error::PayloadTooLarge)?;
    let mut bytes = Vec::with_capacity(capacity);
    let read_bound = expected_length
        .checked_add(1)
        .ok_or(Error::ArithmeticOverflow)?;
    Read::by_ref(&mut file)
        .take(read_bound)
        .read_to_end(&mut bytes)?;
    private_fs::verify_private_file(&file, 1)?;
    if bytes.len() as u64 != expected_length || file.metadata()?.len() != expected_length {
        return Err(Error::ExternalMutation);
    }
    Ok(bytes)
}

fn open_optional_private_dir(parent: &Dir, name: &str) -> Result<Option<Dir>, Error> {
    match parent.symlink_metadata(name) {
        Ok(_) => private_fs::open_private_dir(parent, name)
            .map(Some)
            .map_err(Error::from),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn canonical_manifest_bytes(manifest: &SnapshotManifest) -> Result<Vec<u8>, Error> {
    let bytes = serde_json::to_vec(manifest)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(Error::ManifestTooLarge);
    }
    Ok(bytes)
}

fn validate_ids(snapshot_id: Uuid, vault_id: Uuid) -> Result<(), Error> {
    if snapshot_id.is_nil() {
        return Err(Error::InvalidSnapshotId);
    }
    if vault_id.is_nil() {
        return Err(Error::InvalidVaultId);
    }
    Ok(())
}

#[derive(Deserialize)]
struct RoutingEnvelope {
    version: u32,
    #[serde(deserialize_with = "deserialize_canonical_nonnil_uuid")]
    snapshot_id: Uuid,
    #[serde(deserialize_with = "deserialize_canonical_nonnil_uuid")]
    vault_id: Uuid,
}

fn deserialize_canonical_nonnil_uuid<'de, D>(deserializer: D) -> Result<Uuid, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    let id = Uuid::parse_str(&text).map_err(serde::de::Error::custom)?;
    if id.is_nil() || id.to_string() != text {
        return Err(serde::de::Error::custom(
            "identifier must be a canonical lowercase nonnil UUID",
        ));
    }
    Ok(id)
}

fn sync_published(directory: &Dir, boundary: DurabilityBoundary) -> Result<(), Error> {
    private_fs::sync_directory(directory)
        .map_err(|source| Error::PublishedButNotSynced { boundary, source })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn atomic_rename_noreplace(
    source_parent: &Dir,
    source: &str,
    destination_parent: &Dir,
    destination: &str,
) -> Result<(), Error> {
    let source_held = source_parent.try_clone()?.into_std_file();
    let destination_held = destination_parent.try_clone()?.into_std_file();
    rustix::fs::renameat_with(
        &source_held,
        source,
        &destination_held,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| Error::Io(io::Error::from(error)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn atomic_rename_noreplace(
    _source_parent: &Dir,
    _source: &str,
    _destination_parent: &Dir,
    _destination: &str,
) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod lock_finish_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn end_revalidation_preserves_published_fact_when_named_lock_is_replaced() {
        let temporary = tempfile::tempdir().expect("temporary");
        let base = temporary.path().canonicalize().expect("canonical root");
        let app = base.join("app");
        let vault = base.join("vault");
        fs::create_dir(&app).expect("app");
        fs::create_dir(&vault).expect("vault");
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app");
        let vault_id = Uuid::new_v4();
        let store = SnapshotStore::open(&app, &vault, vault_id).expect("store");
        let operation = store.lock_operation().expect("operation lock");
        let lock_root = app
            .join("recovery-snapshots/v1/vaults")
            .join(vault_id.to_string());
        fs::rename(
            lock_root.join(OPERATION_LOCK_FILE),
            lock_root.join("detached-lock"),
        )
        .expect("detach lock");
        fs::write(lock_root.join(OPERATION_LOCK_FILE), b"").expect("replacement");
        fs::set_permissions(
            lock_root.join(OPERATION_LOCK_FILE),
            fs::Permissions::from_mode(0o600),
        )
        .expect("private replacement");

        assert!(matches!(
            finish_publish_operation(operation, Ok(PublishOutcome::Published)),
            Err(Error::PublishedButLockLost(PublishOutcome::Published))
        ));
    }
}
