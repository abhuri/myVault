#![forbid(unsafe_code)]
#![cfg(target_os = "android")]

use std::{io, io::Write, path::Path};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, File, OpenOptions};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

mod mobile;

/// Opaque native-provenance capability for Android's no-backup directory.
pub struct NativeNoBackupRoot {
    directory: Dir,
    acl: myvault_private_fs::AndroidAclInspection,
}

impl NativeNoBackupRoot {
    /// Reports the root ACL-query result. `Unsupported` is accepted only
    /// because native code is the sole constructor of this capability.
    #[must_use]
    pub fn acl_inspection(&self) -> myvault_private_fs::AndroidAclInspection {
        self.acl
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
        create_or_open_directory(&self.directory, name)
    }

    /// Creates one new private file with exact mode 0600 and nlink one.
    ///
    /// # Errors
    /// Rejects invalid names, collisions, links, ACLs, or durability failures.
    pub fn create_private_file(&self, name: &str) -> Result<NativePrivateFile, PrivateRootError> {
        create_file(&self.directory, name)
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
    fn native_no_backup_root(
        &self,
        vault_root: &Path,
    ) -> Result<NativeNoBackupRoot, PrivateRootError>;
}

impl<R: Runtime, T: Manager<R>> PrivateRootExt<R> for T {
    fn native_no_backup_root(
        &self,
        vault_root: &Path,
    ) -> Result<NativeNoBackupRoot, PrivateRootError> {
        self.state::<mobile::PrivateRoot<R>>().claim(vault_root)
    }
}

#[derive(Debug)]
pub enum PrivateRootError {
    NativeBridge,
    InvalidChildName,
    Validation(myvault_private_fs::Error),
}

impl std::fmt::Display for PrivateRootError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NativeBridge => formatter.write_str("native no-backup bridge failed"),
            Self::InvalidChildName => formatter.write_str("private child name is invalid"),
            Self::Validation(error) => write!(formatter, "invalid native no-backup root: {error}"),
        }
    }
}

impl std::error::Error for PrivateRootError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NativeBridge | Self::InvalidChildName => None,
            Self::Validation(error) => Some(error),
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
