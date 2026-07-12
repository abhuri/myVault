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

const ROOT_DIRECTORY: &str = "recovery-snapshots";
const VERSION_DIRECTORY: &str = "v1";
const VAULTS_DIRECTORY: &str = "vaults";
const BINDING_FILE: &str = "binding.json";
const STAGING_DIRECTORY: &str = "staging";
const OBJECTS_DIRECTORY: &str = "objects";
const MANIFEST_FILE: &str = "manifest.json";
const PAYLOAD_FILE: &str = "payload";

pub const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
pub const MAX_PAYLOAD_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurabilityBoundary {
    BindingDirectory,
    WorkDirectory,
    StagingDirectory,
    ObjectsDirectory,
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
    PublishedButNotSynced {
        boundary: DurabilityBoundary,
        source: private_fs::Error,
    },
}

impl fmt::Display for Error {
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
        let staging = private_fs::create_or_open_private_dir(&vault, STAGING_DIRECTORY)?;
        let objects = private_fs::create_or_open_private_dir(&vault, OBJECTS_DIRECTORY)?;
        Ok(Self {
            _roots: roots,
            vault_id,
            staging,
            objects,
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
            ),
            (None, Some(directory)) => inspect_object(
                &directory,
                EvidenceLocation::Staging,
                snapshot_id,
                self.vault_id,
            ),
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
) -> Result<SnapshotEvidence, Error> {
    verify_exact_object_entries(directory)?;
    let bytes = read_private_file(directory, MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
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
            payload_revision: read_payload_revision(directory)?,
        });
    }
    let manifest: SnapshotManifest = serde_json::from_slice(&bytes)?;
    manifest.validate()?;
    if canonical_manifest_bytes(&manifest)? != bytes
        || read_payload_revision(directory)? != manifest.revision
    {
        return Err(Error::SnapshotCollision);
    }
    Ok(SnapshotEvidence::Supported { location, manifest })
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
    let metadata = directory.symlink_metadata(name)?;
    if !metadata.file_type().is_file() {
        return Err(Error::ExternalMutation);
    }
    if metadata.len() > maximum {
        return Err(if name == PAYLOAD_FILE {
            Error::PayloadTooLarge
        } else {
            Error::ManifestTooLarge
        });
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options)?;
    private_fs::verify_private_file(&file, 1)?;
    let capacity = usize::try_from(metadata.len()).map_err(|_| Error::PayloadTooLarge)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(if name == PAYLOAD_FILE {
            Error::PayloadTooLarge
        } else {
            Error::ManifestTooLarge
        });
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
