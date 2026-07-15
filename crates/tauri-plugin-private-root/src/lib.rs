#![forbid(unsafe_code)]
#![cfg(target_os = "android")]

use std::{io, io::Read, io::Write};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, File, OpenOptions};
use sha2::{Digest, Sha256};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};
use uuid::Uuid;

mod mobile;
mod transfer_store;

pub use transfer_store::{
    AndroidStageWriter, AndroidTransferStore, NativeBaseObjectRef, TransferStoreError,
    VerifiedAndroidStage, MAX_ANDROID_TRANSFER_BYTES,
};

/// Opaque native-provenance capability for Android's no-backup directory.
pub struct NativeNoBackupRoot {
    inspected: myvault_private_fs::InspectedAndroidPrivateRoot,
}

impl NativeNoBackupRoot {
    /// Reports the root ACL-query result. `Unsupported` is accepted only
    /// because native code is the sole constructor of this capability.
    #[must_use]
    pub fn acl_inspection(&self) -> myvault_private_fs::AndroidAclInspection {
        self.inspected.acl_inspection()
    }

    /// Opens crash-safe per-Vault sync state below the native no-backup root.
    /// No ambient path or JavaScript command participates in this operation.
    ///
    /// # Errors
    /// Revalidates native root identity before and after open, and rejects
    /// invalid Vault IDs, unsafe descendants, lease contention, or bad schema.
    pub fn open_sync_store(
        &self,
        vault_id: Uuid,
    ) -> Result<myvault_sync_engine::SyncStore, PrivateRootError> {
        self.inspected
            .revalidate()
            .map_err(PrivateRootError::Validation)?;
        let store = myvault_sync_engine::SyncStore::open_from_android_no_backup_root(
            &self.inspected,
            vault_id,
        )
        .map_err(PrivateRootError::Sync)?;
        self.inspected
            .revalidate()
            .map_err(PrivateRootError::Validation)?;
        Ok(store)
    }

    /// Opens isolated bounded transfer storage for one stable Vault UUID.
    ///
    /// # Errors
    /// Revalidates native no-backup provenance and rejects unsafe descendants,
    /// nil IDs, topology replacement, or durability failures.
    pub fn open_transfer_store(
        &self,
        vault_id: Uuid,
    ) -> transfer_store::Result<AndroidTransferStore> {
        self.inspected.revalidate()?;
        let store = AndroidTransferStore::open(self.inspected.try_clone_directory()?, vault_id)?;
        self.inspected.revalidate()?;
        Ok(store)
    }

    /// Creates or opens one dedicated child directory without following links.
    /// Existing children are validated and never repaired.
    ///
    /// # Errors
    /// Rejects invalid names, unsafe children, ACLs, or durability failures.
    pub fn create_or_open_private_directory(
        &self,
        name: &str,
    ) -> Result<NativePrivateDirectory, PrivateRootError> {
        let directory = self
            .inspected
            .try_clone_directory()
            .map_err(PrivateRootError::Validation)?;
        create_or_open_directory(&directory, name)
    }

    /// Creates one new private file with exact mode 0600 and nlink one.
    ///
    /// # Errors
    /// Rejects invalid names, collisions, links, ACLs, or durability failures.
    pub fn create_private_file(&self, name: &str) -> Result<NativePrivateFile, PrivateRootError> {
        let directory = self
            .inspected
            .try_clone_directory()
            .map_err(PrivateRootError::Validation)?;
        create_file(&directory, name)
    }

    /// Publishes or verifies one immutable content-addressed transfer base
    /// under Android no-backup app data and returns only an opaque digest ref.
    ///
    /// # Errors
    /// Rejects oversized/mismatched content, unsafe private topology, and any
    /// file or directory durability failure.
    pub fn publish_content_addressed_base(
        &self,
        bytes: &[u8],
        expected_sha256_hex: &str,
    ) -> Result<NativeBaseObjectRef, PrivateRootError> {
        let directory = self
            .inspected
            .try_clone_directory()
            .map_err(PrivateRootError::Validation)?;
        publish_content_addressed_base(&directory, bytes, expected_sha256_hex)
    }
}

/// Opaque validated child-directory capability retaining native provenance.
pub struct NativePrivateDirectory {
    directory: Dir,
    acl: myvault_private_fs::AndroidAclInspection,
}

impl NativePrivateDirectory {
    /// Reports the child ACL-query result.
    #[must_use]
    pub fn acl_inspection(&self) -> myvault_private_fs::AndroidAclInspection {
        self.acl
    }

    /// Confirms that the child capability still owns a clonable handle.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.directory.try_clone().is_ok()
    }

    /// Recursively creates or opens a private child directory.
    ///
    /// # Errors
    /// Rejects invalid names, unsafe children, ACLs, or durability failures.
    pub fn create_or_open_private_directory(&self, name: &str) -> Result<Self, PrivateRootError> {
        create_or_open_directory(&self.directory, name)
    }

    /// Creates one new private file below this held directory.
    ///
    /// # Errors
    /// Rejects invalid names, collisions, links, ACLs, or durability failures.
    pub fn create_private_file(&self, name: &str) -> Result<NativePrivateFile, PrivateRootError> {
        create_file(&self.directory, name)
    }
}

/// Opaque validated file capability retaining native provenance.
pub struct NativePrivateFile {
    file: File,
    acl: myvault_private_fs::AndroidAclInspection,
}

impl NativePrivateFile {
    /// Reports the file ACL-query result.
    #[must_use]
    pub fn acl_inspection(&self) -> myvault_private_fs::AndroidAclInspection {
        self.acl
    }

    /// Confirms that the file capability still owns a clonable handle.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.file.try_clone().is_ok()
    }

    /// Writes all bytes, flushes file contents, then revalidates mode, owner,
    /// nlink, and ACL xattrs on the same held file.
    ///
    /// # Errors
    /// Returns an I/O or post-write privacy validation error.
    pub fn write_all_and_sync(&mut self, bytes: &[u8]) -> Result<(), PrivateRootError> {
        self.file.write_all(bytes).map_err(validation_io)?;
        self.file.sync_all().map_err(validation_io)?;
        self.acl = myvault_private_fs::inspect_android_held_file(&self.file)
            .map_err(PrivateRootError::Validation)?;
        Ok(())
    }
}

pub trait PrivateRootExt<R: Runtime> {
    /// Obtains and validates the native Android no-backup root.
    ///
    /// # Errors
    /// Fails when the native bridge or held-root validation fails.
    fn native_no_backup_root(&self) -> Result<NativeNoBackupRoot, PrivateRootError>;
}

impl<R: Runtime, T: Manager<R>> PrivateRootExt<R> for T {
    fn native_no_backup_root(&self) -> Result<NativeNoBackupRoot, PrivateRootError> {
        self.state::<mobile::PrivateRoot<R>>().claim()
    }
}

#[derive(Debug)]
pub enum PrivateRootError {
    NativeBridge,
    InvalidChildName,
    InvalidDigest,
    ResourceLimit,
    DigestMismatch,
    Validation(myvault_private_fs::Error),
    Sync(myvault_sync_engine::Error),
}

impl std::fmt::Display for PrivateRootError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NativeBridge => formatter.write_str("native no-backup bridge failed"),
            Self::InvalidChildName => formatter.write_str("private child name is invalid"),
            Self::InvalidDigest => formatter.write_str("private base digest is invalid"),
            Self::ResourceLimit => formatter.write_str("private base exceeds the byte limit"),
            Self::DigestMismatch => formatter.write_str("private base bytes do not match digest"),
            Self::Validation(error) => write!(formatter, "invalid native no-backup root: {error}"),
            Self::Sync(error) => write!(formatter, "native sync state unavailable: {error}"),
        }
    }
}

impl std::error::Error for PrivateRootError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NativeBridge
            | Self::InvalidChildName
            | Self::InvalidDigest
            | Self::ResourceLimit
            | Self::DigestMismatch => None,
            Self::Validation(error) => Some(error),
            Self::Sync(error) => Some(error),
        }
    }
}

fn validate_child_name(name: &str) -> Result<(), PrivateRootError> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\', '\0']) {
        Err(PrivateRootError::InvalidChildName)
    } else {
        Ok(())
    }
}

fn validation_io(error: io::Error) -> PrivateRootError {
    PrivateRootError::Validation(myvault_private_fs::Error::Io(error))
}

fn create_or_open_directory(
    parent: &Dir,
    name: &str,
) -> Result<NativePrivateDirectory, PrivateRootError> {
    validate_child_name(name)?;
    let created = match parent.create_dir(name) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(validation_io(error)),
    };
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = parent.open_with(name, &options).map_err(validation_io)?;
    if !file.metadata().map_err(validation_io)?.is_dir() {
        return Err(PrivateRootError::Validation(
            myvault_private_fs::Error::ExternalMutation,
        ));
    }
    let directory = Dir::from_std_file(file.into_std());
    if created {
        myvault_private_fs::harden_android_new_directory(&directory)
            .map_err(PrivateRootError::Validation)?;
    }
    let acl = myvault_private_fs::inspect_android_held_directory(&directory)
        .map_err(PrivateRootError::Validation)?;
    if created {
        myvault_private_fs::sync_directory(&directory).map_err(PrivateRootError::Validation)?;
    }
    myvault_private_fs::sync_directory(parent).map_err(PrivateRootError::Validation)?;
    Ok(NativePrivateDirectory { directory, acl })
}

fn create_file(parent: &Dir, name: &str) -> Result<NativePrivateFile, PrivateRootError> {
    validate_child_name(name)?;
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let file = parent.open_with(name, &options).map_err(validation_io)?;
    myvault_private_fs::harden_android_new_file(&file).map_err(PrivateRootError::Validation)?;
    file.sync_all().map_err(validation_io)?;
    let acl = myvault_private_fs::inspect_android_held_file(&file)
        .map_err(PrivateRootError::Validation)?;
    myvault_private_fs::sync_directory(parent).map_err(PrivateRootError::Validation)?;
    Ok(NativePrivateFile { file, acl })
}

fn publish_content_addressed_base(
    root: &Dir,
    bytes: &[u8],
    expected_sha256_hex: &str,
) -> Result<NativeBaseObjectRef, PrivateRootError> {
    const MAX_BASE_BYTES: usize = 16 * 1024 * 1024;
    if bytes.len() > MAX_BASE_BYTES {
        return Err(PrivateRootError::ResourceLimit);
    }
    if expected_sha256_hex.len() != 64
        || !expected_sha256_hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PrivateRootError::InvalidDigest);
    }
    if format!("{:x}", Sha256::digest(bytes)) != expected_sha256_hex {
        return Err(PrivateRootError::DigestMismatch);
    }
    let transfer = create_or_open_directory(root, "guarded-transfer")?;
    let version = create_or_open_directory(&transfer.directory, "v1")?;
    let objects = create_or_open_directory(&version.directory, "objects")?;
    let name = format!("{expected_sha256_hex}.blob");
    match create_file(&objects.directory, &name) {
        Ok(mut file) => file.write_all_and_sync(bytes)?,
        Err(PrivateRootError::Validation(myvault_private_fs::Error::Io(error)))
            if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let mut file = myvault_private_fs::open_private_file(&objects.directory, &name, 1)
        .map_err(PrivateRootError::Validation)?;
    let mut readback = Vec::with_capacity(bytes.len());
    Read::by_ref(&mut file)
        .take(u64::try_from(MAX_BASE_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut readback)
        .map_err(validation_io)?;
    if readback != bytes || format!("{:x}", Sha256::digest(&readback)) != expected_sha256_hex {
        return Err(PrivateRootError::DigestMismatch);
    }
    myvault_private_fs::sync_directory(&objects.directory).map_err(PrivateRootError::Validation)?;
    Ok(NativeBaseObjectRef {
        opaque_ref: format!("sha256-{expected_sha256_hex}"),
        byte_len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    })
}

/// Creates the native-only Tauri plugin. It exposes no JavaScript commands.
#[must_use]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("private-root")
        .setup(|app, api| {
            app.manage(mobile::init(app, &api)?);
            Ok(())
        })
        .build()
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn rejects_path_shaped_and_special_child_names() {
        for name in ["", ".", "..", "a/b", "a\\b", "nul\0name"] {
            assert!(matches!(
                validate_child_name(name),
                Err(PrivateRootError::InvalidChildName)
            ));
        }
        assert!(validate_child_name("operation-journal").is_ok());
    }
}
