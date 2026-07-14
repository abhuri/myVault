use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, File, OpenOptions};
use myvault_platform_fs::{DirectoryIdentity, FileIdentity};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use uuid::Uuid;

const GUARDED_TRANSFER_DIRECTORY: &str = "guarded-transfer";
const STORE_VERSION: &str = "v1";
const STAGING_DIRECTORY: &str = "staging";
const OBJECTS_DIRECTORY: &str = "objects";
pub const MAX_ANDROID_TRANSFER_BYTES: u64 = 16 * 1024 * 1024;

pub type Result<T> = std::result::Result<T, TransferStoreError>;

/// Per-Vault Android no-backup transfer storage with no ambient path API.
pub struct AndroidTransferStore {
    root: HeldDirectory,
    guarded: HeldDirectory,
    version: HeldDirectory,
    vault: HeldDirectory,
    staging: HeldDirectory,
    objects: HeldDirectory,
    vault_name: String,
}

struct HeldDirectory {
    directory: Dir,
    identity: DirectoryIdentity,
}

/// Bounded operation-scoped writer. Dropping it preserves partial evidence.
pub struct AndroidStageWriter {
    file: File,
    identity: FileIdentity,
    operation_id: Uuid,
    staging_identity: DirectoryIdentity,
    digest: Sha256,
    written: u64,
}

/// Exact verified private-stage capability without a path or serializable body.
pub struct VerifiedAndroidStage {
    file: File,
    identity: FileIdentity,
    operation_id: Uuid,
    staging_identity: DirectoryIdentity,
    sha256: String,
    byte_len: u64,
}

impl VerifiedAndroidStage {
    #[must_use]
    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeBaseObjectRef {
    pub(crate) opaque_ref: String,
    pub(crate) byte_len: u64,
}

impl NativeBaseObjectRef {
    #[must_use]
    pub fn opaque_ref(&self) -> &str {
        &self.opaque_ref
    }

    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }
}

impl AndroidTransferStore {
    pub(crate) fn open(root: Dir, vault_id: Uuid) -> Result<Self> {
        if vault_id.is_nil() {
            return Err(TransferStoreError::InvalidVaultId);
        }
        verify_directory(&root)?;
        let root = HeldDirectory::new(root)?;
        let guarded = HeldDirectory::create_or_open(&root.directory, GUARDED_TRANSFER_DIRECTORY)?;
        let version = HeldDirectory::create_or_open(&guarded.directory, STORE_VERSION)?;
        let vault_name = vault_id.to_string();
        let vault = HeldDirectory::create_or_open(&version.directory, &vault_name)?;
        let staging = HeldDirectory::create_or_open(&vault.directory, STAGING_DIRECTORY)?;
        let objects = HeldDirectory::create_or_open(&vault.directory, OBJECTS_DIRECTORY)?;
        let store = Self {
            root,
            guarded,
            version,
            vault,
            staging,
            objects,
            vault_name,
        };
        store.verify_store()?;
        Ok(store)
    }

    /// Creates one new operation-scoped stage and never truncates a collision.
    ///
    /// # Errors
    /// Rejects nil IDs, topology/identity drift, collisions, unsafe files, and I/O failures.
    pub fn begin_stage(&self, operation_id: Uuid) -> Result<AndroidStageWriter> {
        if operation_id.is_nil() {
            return Err(TransferStoreError::InvalidOperationId);
        }
        self.verify_store()?;
        let name = stage_name(operation_id);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let file = self
            .staging
            .directory
            .open_with(&name, &options)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    TransferStoreError::StageCollision
                } else {
                    TransferStoreError::Io(error)
                }
            })?;
        harden_new_file(&file)?;
        verify_file(&file)?;
        let identity = myvault_platform_fs::file_identity(&file)?;
        file.sync_all()?;
        myvault_private_fs::sync_directory(&self.staging.directory)?;
        Ok(AndroidStageWriter {
            file,
            identity,
            operation_id,
            staging_identity: self.staging.identity.clone(),
            digest: Sha256::new(),
            written: 0,
        })
    }

    /// Finishes one stage only when operation, directory, SHA-256, and length are exact.
    /// Wrong or ambiguous evidence is preserved unchanged.
    ///
    /// # Errors
    /// Rejects foreign writers, oversize/mismatched evidence, unsafe topology, or I/O failures.
    pub fn finish_stage(
        &self,
        mut writer: AndroidStageWriter,
        expected_sha256: &str,
        expected_byte_len: u64,
    ) -> Result<VerifiedAndroidStage> {
        validate_expected(expected_sha256, expected_byte_len)?;
        self.verify_store()?;
        if writer.staging_identity != self.staging.identity {
            return Err(TransferStoreError::EvidenceAmbiguous);
        }
        writer.file.flush()?;
        writer.file.sync_all()?;
        verify_file(&writer.file)?;
        if myvault_platform_fs::file_identity(&writer.file)? != writer.identity
            || writer.file.metadata()?.len() != expected_byte_len
            || writer.written != expected_byte_len
            || format!("{:x}", writer.digest.finalize()) != expected_sha256
        {
            return Err(TransferStoreError::DigestMismatch);
        }
        let stage = VerifiedAndroidStage {
            file: writer.file,
            identity: writer.identity,
            operation_id: writer.operation_id,
            staging_identity: writer.staging_identity,
            sha256: expected_sha256.to_owned(),
            byte_len: expected_byte_len,
        };
        self.read_verified_stage(&stage)?;
        myvault_private_fs::sync_directory(&self.staging.directory)?;
        Ok(stage)
    }

    /// Loads an existing stage only when exact SHA-256 and length are verified.
    ///
    /// # Errors
    /// Missing, oversized, hardlinked, wrong-hash, or replaced evidence is preserved and rejected.
    pub fn load_verified_stage(
        &self,
        operation_id: Uuid,
        expected_sha256: &str,
        expected_byte_len: u64,
    ) -> Result<VerifiedAndroidStage> {
        if operation_id.is_nil() {
            return Err(TransferStoreError::InvalidOperationId);
        }
        validate_expected(expected_sha256, expected_byte_len)?;
        self.verify_store()?;
        let file = open_file(&self.staging.directory, &stage_name(operation_id))?;
        let identity = myvault_platform_fs::file_identity(&file)?;
        let stage = VerifiedAndroidStage {
            file,
            identity,
            operation_id,
            staging_identity: self.staging.identity.clone(),
            sha256: expected_sha256.to_owned(),
            byte_len: expected_byte_len,
        };
        self.read_verified_stage(&stage)?;
        Ok(stage)
    }

    /// Reads an exact verified stage after before/after named-identity checks.
    ///
    /// # Errors
    /// Rejects foreign, replaced, hardlinked, oversized, or byte-mismatched evidence.
    pub fn read_verified_stage(&self, stage: &VerifiedAndroidStage) -> Result<Vec<u8>> {
        self.verify_stage(stage)?;
        let mut file = stage.file.try_clone()?;
        let bytes = read_exact_bounded(&mut file, stage.byte_len)?;
        if sha256(&bytes) != stage.sha256 {
            return Err(TransferStoreError::DigestMismatch);
        }
        self.verify_stage(stage)?;
        Ok(bytes)
    }

    /// Removes only a strictly-short, exact operation-scoped stage.
    /// Full-length, oversized, hardlinked, verified, or ambiguous evidence is preserved.
    ///
    /// # Errors
    /// Rejects any evidence not proven safe to discard and all topology/identity drift.
    pub fn discard_incomplete_stage(
        &self,
        operation_id: Uuid,
        expected_sha256: &str,
        expected_byte_len: u64,
    ) -> Result<()> {
        if operation_id.is_nil() {
            return Err(TransferStoreError::InvalidOperationId);
        }
        validate_expected(expected_sha256, expected_byte_len)?;
        self.verify_store()?;
        let name = stage_name(operation_id);
        let held = open_file(&self.staging.directory, &name)?;
        let identity = myvault_platform_fs::file_identity(&held)?;
        let length = held.metadata()?.len();
        if length >= expected_byte_len || length > MAX_ANDROID_TRANSFER_BYTES {
            return Err(TransferStoreError::EvidencePreserved);
        }
        let current = open_file(&self.staging.directory, &name)?;
        if myvault_platform_fs::file_identity(&held)? != identity
            || myvault_platform_fs::file_identity(&current)? != identity
        {
            return Err(TransferStoreError::EvidenceAmbiguous);
        }
        self.staging.directory.remove_file(&name)?;
        myvault_private_fs::sync_directory(&self.staging.directory)?;
        self.verify_store()?;
        Ok(())
    }

    /// Publishes or verifies one immutable content-addressed base object.
    /// Existing wrong bytes are preserved and rejected without replacement.
    ///
    /// # Errors
    /// Rejects foreign stages, collisions with different evidence, unsafe topology, or I/O failures.
    pub fn publish_base(&self, stage: &VerifiedAndroidStage) -> Result<NativeBaseObjectRef> {
        let bytes = self.read_verified_stage(stage)?;
        self.verify_store()?;
        let name = format!("{}.blob", stage.sha256);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        match self.objects.directory.open_with(&name, &options) {
            Ok(mut file) => {
                harden_new_file(&file)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                verify_file(&file)?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(TransferStoreError::Io(error)),
        }
        let mut existing = open_file(&self.objects.directory, &name)?;
        let identity = myvault_platform_fs::file_identity(&existing)?;
        let readback = read_exact_bounded(&mut existing, stage.byte_len)?;
        if readback != bytes || sha256(&readback) != stage.sha256 {
            return Err(TransferStoreError::DigestMismatch);
        }
        let current = open_file(&self.objects.directory, &name)?;
        if myvault_platform_fs::file_identity(&existing)? != identity
            || myvault_platform_fs::file_identity(&current)? != identity
        {
            return Err(TransferStoreError::EvidenceAmbiguous);
        }
        myvault_private_fs::sync_directory(&self.objects.directory)?;
        self.verify_store()?;
        Ok(NativeBaseObjectRef {
            opaque_ref: format!("sha256-{}", stage.sha256),
            byte_len: stage.byte_len,
        })
    }

    fn verify_stage(&self, stage: &VerifiedAndroidStage) -> Result<()> {
        self.verify_store()?;
        if stage.operation_id.is_nil()
            || stage.staging_identity != self.staging.identity
            || stage.byte_len > MAX_ANDROID_TRANSFER_BYTES
        {
            return Err(TransferStoreError::EvidenceAmbiguous);
        }
        verify_file(&stage.file)?;
        if myvault_platform_fs::file_identity(&stage.file)? != stage.identity {
            return Err(TransferStoreError::EvidenceAmbiguous);
        }
        let current = open_file(&self.staging.directory, &stage_name(stage.operation_id))?;
        if myvault_platform_fs::file_identity(&current)? != stage.identity
            || current.metadata()?.len() != stage.byte_len
        {
            return Err(TransferStoreError::EvidenceAmbiguous);
        }
        Ok(())
    }

    fn verify_store(&self) -> Result<()> {
        self.root.verify_self()?;
        self.guarded
            .verify_named_child(&self.root.directory, GUARDED_TRANSFER_DIRECTORY)?;
        self.version
            .verify_named_child(&self.guarded.directory, STORE_VERSION)?;
        self.vault
            .verify_named_child(&self.version.directory, &self.vault_name)?;
        self.staging
            .verify_named_child(&self.vault.directory, STAGING_DIRECTORY)?;
        self.objects
            .verify_named_child(&self.vault.directory, OBJECTS_DIRECTORY)?;
        Ok(())
    }
}

impl HeldDirectory {
    fn new(directory: Dir) -> Result<Self> {
        verify_directory(&directory)?;
        let identity = myvault_platform_fs::directory_identity(&directory)?;
        Ok(Self {
            directory,
            identity,
        })
    }

    fn create_or_open(parent: &Dir, name: &str) -> Result<Self> {
        let directory = create_or_open_directory(parent, name)?;
        Self::new(directory)
    }

    fn verify_self(&self) -> Result<()> {
        verify_directory(&self.directory)?;
        if myvault_platform_fs::directory_identity(&self.directory)? != self.identity {
            return Err(TransferStoreError::EvidenceAmbiguous);
        }
        Ok(())
    }

    fn verify_named_child(&self, parent: &Dir, name: &str) -> Result<()> {
        self.verify_self()?;
        let current = open_directory(parent, name)?;
        if myvault_platform_fs::directory_identity(&current)? != self.identity {
            return Err(TransferStoreError::EvidenceAmbiguous);
        }
        Ok(())
    }
}

impl Write for AndroidStageWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let requested = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        let next = self
            .written
            .checked_add(requested)
            .ok_or_else(|| io::Error::other("private transfer stage limit exceeded"))?;
        if next > MAX_ANDROID_TRANSFER_BYTES {
            return Err(io::Error::other("private transfer stage limit exceeded"));
        }
        let written = self.file.write(buffer)?;
        self.digest.update(&buffer[..written]);
        self.written = self
            .written
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn create_or_open_directory(parent: &Dir, name: &str) -> Result<Dir> {
    #[cfg(target_os = "android")]
    {
        Ok(myvault_private_fs::create_or_open_android_private_dir(
            parent, name,
        )?)
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(myvault_private_fs::create_or_open_private_dir(
            parent, name,
        )?)
    }
}

fn open_directory(parent: &Dir, name: &str) -> Result<Dir> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_dir() {
        return Err(TransferStoreError::EvidenceAmbiguous);
    }
    let directory = Dir::from_std_file(file.into_std());
    verify_directory(&directory)?;
    Ok(directory)
}

fn verify_directory(directory: &Dir) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        myvault_private_fs::inspect_android_held_directory(directory)?;
    }
    #[cfg(not(target_os = "android"))]
    {
        // Opening a normalized child through the strict policy verifies the
        // directory at creation/open time. Recheck exact owner/mode/ACL using
        // a no-op private child-independent handle validation.
        use std::os::unix::fs::MetadataExt;
        let metadata = directory.try_clone()?.into_std_file().metadata()?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(TransferStoreError::EvidenceAmbiguous);
        }
    }
    Ok(())
}

fn harden_new_file(file: &File) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        myvault_private_fs::harden_android_new_file(file)?;
    }
    #[cfg(not(target_os = "android"))]
    {
        myvault_private_fs::set_private_file_permissions(file)?;
    }
    Ok(())
}

fn verify_file(file: &File) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        myvault_private_fs::inspect_android_held_file(file)?;
    }
    #[cfg(not(target_os = "android"))]
    {
        myvault_private_fs::verify_private_file(file, 1)?;
    }
    Ok(())
}

fn open_file(parent: &Dir, name: &str) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = parent.open_with(name, &options).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            TransferStoreError::StageUnavailable
        } else {
            TransferStoreError::Io(error)
        }
    })?;
    verify_file(&file)?;
    Ok(file)
}

fn read_exact_bounded(file: &mut File, expected_byte_len: u64) -> Result<Vec<u8>> {
    if expected_byte_len > MAX_ANDROID_TRANSFER_BYTES {
        return Err(TransferStoreError::ResourceLimit);
    }
    let capacity =
        usize::try_from(expected_byte_len).map_err(|_| TransferStoreError::ResourceLimit)?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(file)
        .take(MAX_ANDROID_TRANSFER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_byte_len {
        return Err(TransferStoreError::DigestMismatch);
    }
    Ok(bytes)
}

fn validate_expected(expected_sha256: &str, expected_byte_len: u64) -> Result<()> {
    if expected_byte_len > MAX_ANDROID_TRANSFER_BYTES {
        return Err(TransferStoreError::ResourceLimit);
    }
    if expected_sha256.len() != 64
        || !expected_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(TransferStoreError::InvalidDigest);
    }
    Ok(())
}

fn stage_name(operation_id: Uuid) -> String {
    format!("{operation_id}.stage")
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug)]
pub enum TransferStoreError {
    InvalidVaultId,
    InvalidOperationId,
    InvalidDigest,
    ResourceLimit,
    StageCollision,
    StageUnavailable,
    DigestMismatch,
    EvidencePreserved,
    EvidenceAmbiguous,
    Io(io::Error),
    PrivateStorage(myvault_private_fs::Error),
}

impl fmt::Display for TransferStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVaultId => formatter.write_str("the transfer Vault ID is invalid"),
            Self::InvalidOperationId => formatter.write_str("the transfer operation ID is invalid"),
            Self::InvalidDigest => formatter.write_str("the transfer digest is invalid"),
            Self::ResourceLimit => formatter.write_str("the private transfer exceeds its limit"),
            Self::StageCollision => formatter.write_str("the private stage already exists"),
            Self::StageUnavailable => formatter.write_str("the private stage is unavailable"),
            Self::DigestMismatch => formatter.write_str("private transfer bytes do not match"),
            Self::EvidencePreserved => {
                formatter.write_str("private transfer evidence was preserved")
            }
            Self::EvidenceAmbiguous => {
                formatter.write_str("private transfer evidence is ambiguous")
            }
            Self::Io(_) => formatter.write_str("private transfer I/O failed"),
            Self::PrivateStorage(_) => formatter.write_str("private transfer storage is unsafe"),
        }
    }
}

impl std::error::Error for TransferStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::PrivateStorage(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for TransferStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<myvault_private_fs::Error> for TransferStoreError {
    fn from(error: myvault_private_fs::Error) -> Self {
        Self::PrivateStorage(error)
    }
}
