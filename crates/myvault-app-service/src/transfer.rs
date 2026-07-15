use std::{
    io::{Seek, SeekFrom, Write},
    path::Path,
};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File, OpenOptions};
use myvault_core::{
    stream_content_snapshot, ContentPublishOutcome, ContentSnapshot, CoreError, FileRevision,
    MoveDurability, Sha256Digest, Vault, VaultPath,
};
use myvault_private_fs as private_fs;
use uuid::Uuid;

const STORE_DIRECTORY: &str = "guarded-transfer";
const STORE_VERSION: &str = "v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferStageRef {
    operation_id: Uuid,
    snapshot: ContentSnapshot,
}

impl TransferStageRef {
    #[must_use]
    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    #[must_use]
    pub const fn snapshot(&self) -> &ContentSnapshot {
        &self.snapshot
    }
}

/// Native-only bounded sink. It owns a descriptor-relative private file and
/// has no ambient path, debug output, or serialization surface.
pub struct TransferStageWriter {
    file: Option<File>,
    operation_id: Uuid,
    store_identity: private_fs::HeldDirectoryIdentity,
    vault_id: Uuid,
    written: u64,
    max_bytes: u64,
}

impl Write for TransferStageWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .written
            .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| std::io::Error::other("private transfer stage limit exceeded"))?;
        if next > self.max_bytes {
            return Err(std::io::Error::other(
                "private transfer stage limit exceeded",
            ));
        }
        let count = self
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("private transfer stage is closed"))?
            .write(bytes)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("private transfer stage is closed"))?
            .flush()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferBaseRef {
    sha256: Sha256Digest,
    byte_len: u64,
}

impl TransferBaseRef {
    #[must_use]
    pub fn opaque_ref(&self) -> String {
        format!("sha256-{}", self.sha256.as_str())
    }

    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeTransferPublication {
    pub snapshot: ContentSnapshot,
    pub base_ref: TransferBaseRef,
    pub durability: MoveDurability,
}

#[derive(Debug)]
pub enum NativeTransferError {
    InvalidRequest,
    ProtectedPath,
    StaleRevision,
    UnsupportedReplace,
    DigestMismatch,
    ResourceLimit,
    StageUnavailable,
    StageAlreadyExists,
    PrivateStoreUnavailable,
    PublicationUnknown,
    VaultUnavailable,
}

impl std::fmt::Display for NativeTransferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest => "the native transfer request is invalid",
            Self::ProtectedPath => "the native transfer path is protected",
            Self::StaleRevision => "the local transfer revision is stale",
            Self::UnsupportedReplace => "existing-target transfer is unsupported in R2",
            Self::DigestMismatch => "the transfer bytes do not match their declared digest",
            Self::ResourceLimit => "the transfer exceeds its native byte limit",
            Self::StageUnavailable => "the private transfer stage is unavailable",
            Self::StageAlreadyExists => "the private transfer operation already has evidence",
            Self::PrivateStoreUnavailable => "the private transfer store is unavailable",
            Self::PublicationUnknown => "the local transfer publication outcome is unknown",
            Self::VaultUnavailable => "the local Vault capability is unavailable",
        })
    }
}

impl std::error::Error for NativeTransferError {}

pub(crate) struct PrivateTransferStore {
    root: Dir,
    root_identity: private_fs::HeldDirectoryIdentity,
    staging: Dir,
    staging_identity: private_fs::HeldDirectoryIdentity,
    objects: Dir,
    objects_identity: private_fs::HeldDirectoryIdentity,
    vault_id: Uuid,
}

impl PrivateTransferStore {
    pub(crate) fn open(
        app_data_root: &Path,
        vault_root: &Path,
        vault_id: Uuid,
    ) -> Result<Self, NativeTransferError> {
        if vault_id.is_nil() {
            return Err(NativeTransferError::InvalidRequest);
        }
        let app = private_fs::open_private_disjoint_root(app_data_root, vault_root)
            .map_err(map_private_error)?;
        let product = private_fs::create_or_open_private_dir(&app, STORE_DIRECTORY)
            .map_err(map_private_error)?;
        let version = private_fs::create_or_open_private_dir(&product, STORE_VERSION)
            .map_err(map_private_error)?;
        let root = private_fs::create_or_open_private_dir(&version, vault_id.to_string())
            .map_err(map_private_error)?;
        let staging =
            private_fs::create_or_open_private_dir(&root, "staging").map_err(map_private_error)?;
        let objects =
            private_fs::create_or_open_private_dir(&root, "objects").map_err(map_private_error)?;
        let root_identity =
            private_fs::held_directory_identity(&root).map_err(map_private_error)?;
        let staging_identity =
            private_fs::held_directory_identity(&staging).map_err(map_private_error)?;
        let objects_identity =
            private_fs::held_directory_identity(&objects).map_err(map_private_error)?;
        Ok(Self {
            root,
            root_identity,
            staging,
            staging_identity,
            objects,
            objects_identity,
            vault_id,
        })
    }

    pub(crate) fn begin_stage(
        &self,
        operation_id: Uuid,
        max_bytes: usize,
    ) -> Result<TransferStageWriter, NativeTransferError> {
        if operation_id.is_nil() {
            return Err(NativeTransferError::InvalidRequest);
        }
        self.verify_root()?;
        let name = stage_name(operation_id);
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let file = self.staging.open_with(&name, &options).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                NativeTransferError::StageAlreadyExists
            } else {
                NativeTransferError::PrivateStoreUnavailable
            }
        })?;
        private_fs::set_private_file_permissions(&file).map_err(map_private_error)?;
        private_fs::verify_private_file(&file, 1).map_err(map_private_error)?;
        file.sync_all()
            .map_err(|_| NativeTransferError::PrivateStoreUnavailable)?;
        private_fs::sync_directory(&self.staging).map_err(map_private_error)?;
        Ok(TransferStageWriter {
            file: Some(file),
            operation_id,
            store_identity: self.root_identity.clone(),
            vault_id: self.vault_id,
            written: 0,
            max_bytes: u64::try_from(max_bytes).unwrap_or(u64::MAX),
        })
    }

    pub(crate) fn finish_stage(
        &self,
        mut writer: TransferStageWriter,
        expected_sha256: &Sha256Digest,
        expected_byte_len: u64,
        max_bytes: usize,
    ) -> Result<TransferStageRef, NativeTransferError> {
        if writer.store_identity != self.root_identity
            || writer.vault_id != self.vault_id
            || writer.written != expected_byte_len
            || writer.written > u64::try_from(max_bytes).unwrap_or(u64::MAX)
        {
            return Err(NativeTransferError::DigestMismatch);
        }
        let file = writer
            .file
            .take()
            .ok_or(NativeTransferError::StageUnavailable)?;
        file.sync_all()
            .map_err(|_| NativeTransferError::PrivateStoreUnavailable)?;
        private_fs::verify_private_file(&file, 1).map_err(map_private_error)?;
        drop(file);
        private_fs::sync_directory(&self.staging).map_err(map_private_error)?;
        self.load_stage(
            writer.operation_id,
            expected_sha256,
            expected_byte_len,
            max_bytes,
        )
    }

    pub(crate) fn load_stage(
        &self,
        operation_id: Uuid,
        expected_sha256: &Sha256Digest,
        expected_byte_len: u64,
        max_bytes: usize,
    ) -> Result<TransferStageRef, NativeTransferError> {
        if operation_id.is_nil() || expected_byte_len > u64::try_from(max_bytes).unwrap_or(u64::MAX)
        {
            return Err(NativeTransferError::InvalidRequest);
        }
        self.verify_root()?;
        let mut file = self.open_stage(operation_id, 2)?;
        let mut sink = std::io::sink();
        let snapshot =
            stream_content_snapshot(&mut file, &mut sink, max_bytes).map_err(map_core_error)?;
        if snapshot.sha256 != *expected_sha256 || snapshot.byte_len != expected_byte_len {
            return Err(NativeTransferError::DigestMismatch);
        }
        Ok(TransferStageRef {
            operation_id,
            snapshot,
        })
    }

    pub(crate) fn discard_incomplete_stage(
        &self,
        operation_id: Uuid,
        expected_sha256: &Sha256Digest,
        expected_byte_len: u64,
        max_bytes: usize,
    ) -> Result<(), NativeTransferError> {
        if operation_id.is_nil() || expected_byte_len > u64::try_from(max_bytes).unwrap_or(u64::MAX)
        {
            return Err(NativeTransferError::InvalidRequest);
        }
        self.verify_root()?;
        let name = stage_name(operation_id);
        let mut file = self.open_stage(operation_id, 1)?;
        let identity = private_fs::held_private_file_identity(&file).map_err(map_private_error)?;
        let mut sink = std::io::sink();
        let snapshot =
            stream_content_snapshot(&mut file, &mut sink, max_bytes).map_err(map_core_error)?;
        if snapshot.sha256 == *expected_sha256 && snapshot.byte_len == expected_byte_len {
            // Exact evidence may already be usable by the durable operation.
            // Recovery must never turn a verified stage into a fresh download.
            return Err(NativeTransferError::StageAlreadyExists);
        }
        if snapshot.byte_len >= expected_byte_len {
            // A full-length wrong digest (or oversized evidence) is corruption,
            // not a known-interrupted stream. Preserve it for reconciliation.
            return Err(NativeTransferError::DigestMismatch);
        }
        self.verify_root()?;
        private_fs::remove_private_file_if_identity(&self.staging, name, &file, &identity)
            .map_err(|error| match error {
                private_fs::Error::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    NativeTransferError::StageUnavailable
                }
                other => map_private_error(other),
            })?;
        private_fs::sync_directory(&self.staging).map_err(map_private_error)
    }

    pub(crate) fn open_verified_stage(
        &self,
        stage: &TransferStageRef,
        max_bytes: usize,
    ) -> Result<File, NativeTransferError> {
        let verified = self.load_stage(
            stage.operation_id,
            &stage.snapshot.sha256,
            stage.snapshot.byte_len,
            max_bytes,
        )?;
        if verified != *stage {
            return Err(NativeTransferError::DigestMismatch);
        }
        let mut file = self.open_stage(stage.operation_id, 2)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| NativeTransferError::StageUnavailable)?;
        Ok(file)
    }

    pub(crate) fn publish_base(
        &self,
        stage: &TransferStageRef,
        max_bytes: usize,
    ) -> Result<TransferBaseRef, NativeTransferError> {
        self.verify_root()?;
        let stage_file = self.open_verified_stage(stage, max_bytes)?;
        let object_name = object_name(&stage.snapshot.sha256);
        let object = match private_fs::open_private_file(&self.objects, &object_name, 2) {
            Ok(object) => {
                let stage_links = file_link_count(&stage_file)?;
                if stage_links > 1 && !same_file_identity(&stage_file, &object)? {
                    return Err(NativeTransferError::PublicationUnknown);
                }
                object
            }
            Err(private_fs::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                private_fs::verify_private_file(&stage_file, 1).map_err(map_private_error)?;
                match self.staging.hard_link(
                    stage_name(stage.operation_id),
                    &self.objects,
                    &object_name,
                ) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return self.publish_base(stage, max_bytes);
                    }
                    Err(_) => return Err(NativeTransferError::PublicationUnknown),
                }
                private_fs::sync_directory(&self.objects).map_err(map_private_error)?;
                let object = private_fs::open_private_file(&self.objects, &object_name, 2)
                    .map_err(map_private_error)?;
                if !same_file_identity(&stage_file, &object)? {
                    return Err(NativeTransferError::PublicationUnknown);
                }
                object
            }
            Err(error) => return Err(map_private_error(error)),
        };
        verify_file_snapshot(&object, &stage.snapshot, max_bytes)?;
        let current = self.open_stage(stage.operation_id, 2)?;
        if !same_file_identity(&stage_file, &current)? {
            return Err(NativeTransferError::PublicationUnknown);
        }
        self.staging
            .remove_file(stage_name(stage.operation_id))
            .map_err(|_| NativeTransferError::PublicationUnknown)?;
        private_fs::sync_directory(&self.staging).map_err(map_private_error)?;
        let object = private_fs::open_private_file(&self.objects, &object_name, 1)
            .map_err(map_private_error)?;
        verify_file_snapshot(&object, &stage.snapshot, max_bytes)?;
        Ok(TransferBaseRef {
            sha256: stage.snapshot.sha256.clone(),
            byte_len: stage.snapshot.byte_len,
        })
    }

    pub(crate) fn open_base(
        &self,
        base: &TransferBaseRef,
        max_bytes: usize,
    ) -> Result<File, NativeTransferError> {
        let object = private_fs::open_private_file(&self.objects, object_name(&base.sha256), 1)
            .map_err(map_private_error)?;
        let mut file = object;
        let mut sink = std::io::sink();
        let snapshot =
            stream_content_snapshot(&mut file, &mut sink, max_bytes).map_err(map_core_error)?;
        if snapshot.sha256 != base.sha256 || snapshot.byte_len != base.byte_len {
            return Err(NativeTransferError::DigestMismatch);
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| NativeTransferError::PrivateStoreUnavailable)?;
        Ok(file)
    }

    fn open_stage(&self, operation_id: Uuid, max_links: u64) -> Result<File, NativeTransferError> {
        private_fs::open_private_file(&self.staging, stage_name(operation_id), max_links).map_err(
            |error| match error {
                private_fs::Error::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    NativeTransferError::StageUnavailable
                }
                other => map_private_error(other),
            },
        )
    }

    fn verify_root(&self) -> Result<(), NativeTransferError> {
        if private_fs::held_directory_identity(&self.root).map_err(map_private_error)?
            != self.root_identity
        {
            return Err(NativeTransferError::PrivateStoreUnavailable);
        }
        let staging =
            private_fs::open_private_dir(&self.root, "staging").map_err(map_private_error)?;
        let objects =
            private_fs::open_private_dir(&self.root, "objects").map_err(map_private_error)?;
        if private_fs::held_directory_identity(&staging).map_err(map_private_error)?
            != self.staging_identity
            || private_fs::held_directory_identity(&objects).map_err(map_private_error)?
                != self.objects_identity
        {
            return Err(NativeTransferError::PrivateStoreUnavailable);
        }
        Ok(())
    }
}

pub(crate) fn publish_stage(
    vault: &Vault,
    store: &PrivateTransferStore,
    stage: &TransferStageRef,
    path: &VaultPath,
    expected_revision: Option<&FileRevision>,
    max_bytes: usize,
) -> Result<NativeTransferPublication, NativeTransferError> {
    // Base publication precedes the Vault side effect. A base failure can
    // therefore never leave an untracked local create.
    let base_ref = store
        .publish_base(stage, max_bytes)
        .map_err(|_| NativeTransferError::PublicationUnknown)?;
    let mut reader = store.open_base(&base_ref, max_bytes)?;
    let outcome = match expected_revision {
        Some(expected) => vault.replace_content_from_reader_if_revision(
            path,
            expected,
            &mut reader,
            &stage.snapshot.sha256,
            stage.snapshot.byte_len,
            max_bytes,
        ),
        None => vault.create_content_from_reader(
            path,
            &mut reader,
            &stage.snapshot.sha256,
            stage.snapshot.byte_len,
            max_bytes,
        ),
    }
    .map_err(map_core_error)?;
    let durability = match outcome {
        ContentPublishOutcome::Created(value) | ContentPublishOutcome::Replaced(value) => value,
    };
    Ok(NativeTransferPublication {
        snapshot: stage.snapshot.clone(),
        base_ref,
        durability,
    })
}

fn stage_name(operation_id: Uuid) -> String {
    format!("{operation_id}.part")
}

fn object_name(digest: &Sha256Digest) -> String {
    format!("{}.blob", digest.as_str())
}

fn verify_file_snapshot(
    file: &File,
    expected: &ContentSnapshot,
    max_bytes: usize,
) -> Result<(), NativeTransferError> {
    let mut file = file
        .try_clone()
        .map_err(|_| NativeTransferError::PrivateStoreUnavailable)?;
    let mut sink = std::io::sink();
    let actual =
        stream_content_snapshot(&mut file, &mut sink, max_bytes).map_err(map_core_error)?;
    if &actual == expected {
        Ok(())
    } else {
        Err(NativeTransferError::DigestMismatch)
    }
}

fn same_file_identity(left: &File, right: &File) -> Result<bool, NativeTransferError> {
    let left = myvault_platform_fs::file_identity(left)
        .map_err(|_| NativeTransferError::PrivateStoreUnavailable)?;
    let right = myvault_platform_fs::file_identity(right)
        .map_err(|_| NativeTransferError::PrivateStoreUnavailable)?;
    Ok(left == right)
}

#[cfg(unix)]
fn file_link_count(file: &File) -> Result<u64, NativeTransferError> {
    use std::os::unix::fs::MetadataExt;
    file.try_clone()
        .and_then(|file| file.into_std().metadata())
        .map(|metadata| metadata.nlink())
        .map_err(|_| NativeTransferError::PrivateStoreUnavailable)
}

#[cfg(not(unix))]
fn file_link_count(_file: &File) -> Result<u64, NativeTransferError> {
    Ok(1)
}

fn map_private_error(_error: private_fs::Error) -> NativeTransferError {
    NativeTransferError::PrivateStoreUnavailable
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn map_core_error(error: CoreError) -> NativeTransferError {
    match error {
        CoreError::InvalidRelativePath(_)
        | CoreError::PathEscapesVault(_)
        | CoreError::SymlinkRejected(_)
        | CoreError::AutomaticObsidianWriteDenied(_)
        | CoreError::TrashWriteDenied(_)
        | CoreError::TrashAccessDenied(_)
        | CoreError::InvalidMove { .. } => NativeTransferError::ProtectedPath,
        CoreError::StaleRevision { .. } | CoreError::AlreadyExists(_) => {
            NativeTransferError::StaleRevision
        }
        CoreError::ExistingContentReplaceUnsupported => NativeTransferError::UnsupportedReplace,
        CoreError::InvalidSha256Digest | CoreError::InvalidRevision => {
            NativeTransferError::InvalidRequest
        }
        CoreError::ContentDigestMismatch => NativeTransferError::DigestMismatch,
        CoreError::ResourceLimitExceeded { .. } => NativeTransferError::ResourceLimit,
        CoreError::CommitOutcomeUnknown { .. }
        | CoreError::PublishedCleanupPending { .. }
        | CoreError::ReplaceContentOutcomeUnknown { .. } => NativeTransferError::PublicationUnknown,
        _ => NativeTransferError::VaultUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cap_std::{ambient_authority, fs::Dir};

    use super::same_file_identity;

    #[test]
    fn handle_identity_matches_hard_link_but_not_equal_bytes() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let original = temporary.path().join("original");
        let hard_link = temporary.path().join("hard-link");
        let equal_copy = temporary.path().join("equal-copy");
        fs::write(&original, b"same bytes").expect("write original");
        fs::hard_link(&original, &hard_link).expect("create hard link");
        fs::write(&equal_copy, b"same bytes").expect("write equal copy");

        let directory =
            Dir::open_ambient_dir(temporary.path(), ambient_authority()).expect("open directory");
        let original = directory.open("original").expect("open original");
        let hard_link = directory.open("hard-link").expect("open hard link");
        let equal_copy = directory.open("equal-copy").expect("open equal copy");

        assert!(same_file_identity(&original, &hard_link).expect("compare hard link"));
        assert!(!same_file_identity(&original, &equal_copy).expect("compare equal copy"));
    }
}
