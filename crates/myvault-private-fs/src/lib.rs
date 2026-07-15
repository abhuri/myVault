#![forbid(unsafe_code)]

//! Held-capability helpers for private application data.
//!
//! Supported Unix hosts validate ownership, modes, links, and extended ACLs.
//! Other targets fail closed until an equivalent privacy proof exists.

use cap_fs_ext::OpenOptionsMaybeDirExt;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::OpenOptions;
use cap_std::fs::{Dir, File};
use std::fmt;
#[cfg(unix)]
use std::fs;
use std::io;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    DirectorySyncUnsupported(io::Error),
    InvalidRoot(&'static str),
    PrivacyValidationRequired,
    ExtendedAcl,
    ExternalMutation,
}

#[cfg(target_os = "android")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AndroidAclInspection {
    Clean,
    Unsupported,
}

/// A held Android directory whose filesystem facts were inspected without
/// claiming native no-backup provenance.
#[cfg(target_os = "android")]
pub struct InspectedAndroidPrivateRoot {
    directory: Dir,
    canonical_path: PathBuf,
    identity: HeldDirectoryIdentity,
    acl: AndroidAclInspection,
}

/// Stable Unix identity facts read from a held root capability.
///
/// These facts are intentionally narrow: they are suitable for binding
/// immutable app data to one vault root, but they do not expose an ambient
/// path or authorize filesystem access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnixRootIdentity {
    device: u64,
    inode: u64,
}

impl UnixRootIdentity {
    #[must_use]
    pub const fn device(self) -> u64 {
        self.device
    }

    #[must_use]
    pub const fn inode(self) -> u64 {
        self.inode
    }
}

/// Held roots and identity evidence from one race-checked disjoint-root open.
/// Keeping both handles alive lets consumers bind later evidence to the exact
/// roots that passed validation.
pub struct PrivateDisjointRoots {
    private_root: Dir,
    other_root: Dir,
    other_identity: UnixRootIdentity,
}

/// A race-checked private root capability retaining its canonical location and
/// the identity of the held directory.
///
/// Private fields prevent downstream code from assembling this capability
/// from an arbitrary ambient path.
pub struct HeldPrivateRoot {
    directory: Dir,
    canonical_path: PathBuf,
    identity: HeldDirectoryIdentity,
}

/// Opaque identity of a held directory, suitable for detecting replacement
/// across an atomic rename without exposing platform-specific identifiers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeldDirectoryIdentity(myvault_platform_fs::DirectoryIdentity);

/// Opaque identity of a held private regular file.
#[derive(Debug, Eq, PartialEq)]
pub struct HeldPrivateFileIdentity(myvault_platform_fs::FileIdentity);

/// Captures the platform-complete identity of a held directory capability.
///
/// # Errors
/// Fails closed when the platform cannot provide a complete held identity.
pub fn held_directory_identity(directory: &Dir) -> Result<HeldDirectoryIdentity, Error> {
    myvault_platform_fs::directory_identity(directory)
        .map(HeldDirectoryIdentity)
        .map_err(Error::Io)
}

/// Captures the identity of a held private regular file with exactly one link.
///
/// # Errors
/// Rejects insecure, non-regular, hardlinked, or unsupported held files.
pub fn held_private_file_identity(file: &File) -> Result<HeldPrivateFileIdentity, Error> {
    verify_private_file(file, 1)?;
    myvault_platform_fs::file_identity(file)
        .map(HeldPrivateFileIdentity)
        .map_err(Error::Io)
}

/// Opens one existing private regular file relative to a held parent.
///
/// # Errors
/// Fails for invalid names, symlinks/reparse points, wrong type/privacy, or links.
pub fn open_private_file(
    parent: &Dir,
    name: impl AsRef<Path>,
    max_links: u64,
) -> Result<File, Error> {
    let name = name.as_ref();
    validate_child_name(name)?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = parent.open_with(name, &options)?;
    verify_private_file(&file, max_links)?;
    Ok(file)
}

/// Removes one private regular file only when its original held handle, token,
/// and the current named handle all still identify the same private file.
///
/// Cooperating processes must serialize mutations with the operation lock. A
/// malicious same-UID syscall race remains outside the threat model because
/// Unix and macOS provide no portable unlink-by-handle primitive.
///
/// # Errors
/// Preserves a distinct `NotFound` I/O error and rejects every identity/topology mismatch.
pub fn remove_private_file_if_identity(
    parent: &Dir,
    name: impl AsRef<Path>,
    held: &File,
    expected: &HeldPrivateFileIdentity,
) -> Result<(), Error> {
    let name = name.as_ref();
    let current = open_private_file(parent, name, 1)?;
    if &held_private_file_identity(held)? != expected {
        return Err(Error::ExternalMutation);
    }
    if &held_private_file_identity(&current)? != expected {
        return Err(Error::ExternalMutation);
    }
    parent.remove_file(name)?;
    Ok(())
}

/// Removes one empty private directory only when its original held handle,
/// token, and the current named handle all still identify the same empty child.
///
/// Cooperating processes must serialize mutations with the operation lock. A
/// malicious same-UID syscall race remains outside the threat model because
/// Unix and macOS provide no portable unlink-by-handle primitive.
///
/// # Errors
/// Rejects nonempty, replaced, symlinked/reparse, insecure, or invalid children.
pub fn remove_empty_private_dir_if_identity(
    parent: &Dir,
    name: impl AsRef<Path>,
    held: &Dir,
    expected: &HeldDirectoryIdentity,
) -> Result<(), Error> {
    let name = name.as_ref();
    validate_child_name(name)?;
    if &held_directory_identity(held)? != expected || held.entries()?.next().transpose()?.is_some()
    {
        return Err(Error::ExternalMutation);
    }
    let current = open_private_dir(parent, name)?;
    if &held_directory_identity(&current)? != expected {
        return Err(Error::ExternalMutation);
    }
    if current.entries()?.next().transpose()?.is_some() {
        return Err(Error::ExternalMutation);
    }
    parent.remove_dir(name)?;
    Ok(())
}

impl PrivateDisjointRoots {
    #[must_use]
    pub const fn private_root(&self) -> &Dir {
        &self.private_root
    }

    #[must_use]
    pub const fn other_root(&self) -> &Dir {
        &self.other_root
    }

    #[must_use]
    pub const fn other_identity(&self) -> UnixRootIdentity {
        self.other_identity
    }
}

impl HeldPrivateRoot {
    /// Revalidates privacy and the exact held/canonical directory identity.
    ///
    /// # Errors
    /// Rejects replacement, rename, symlink substitution, permission drift,
    /// extended ACLs, and platforms without a complete privacy proof.
    pub fn revalidate(&self) -> Result<(), Error> {
        let canonical = self.canonical_path.canonicalize()?;
        if canonical != self.canonical_path {
            return Err(Error::ExternalMutation);
        }
        require_private_directory(&self.directory)?;
        verify_root_identity(&self.directory, &self.canonical_path)?;
        if held_directory_identity(&self.directory)? != self.identity {
            return Err(Error::ExternalMutation);
        }
        Ok(())
    }

    /// Clones the held directory capability without performing an ambient open.
    ///
    /// # Errors
    /// Returns an I/O error if the descriptor can no longer be cloned.
    #[doc(hidden)]
    pub fn try_clone_directory(&self) -> Result<Dir, Error> {
        Ok(self.directory.try_clone()?)
    }

    /// Returns the canonical location retained by the validated capability.
    #[doc(hidden)]
    #[must_use]
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }
}

#[cfg(target_os = "android")]
impl InspectedAndroidPrivateRoot {
    /// Returns whether ACL xattrs were proven absent or unsupported.
    #[must_use]
    pub fn acl_inspection(&self) -> AndroidAclInspection {
        self.acl
    }

    /// Revalidates exact mode/owner/ACL facts and held/canonical identity.
    ///
    /// # Errors
    /// Rejects path replacement, symlinks, ownership/mode drift, or ACL drift.
    pub fn revalidate(&self) -> Result<(), Error> {
        let canonical = self.canonical_path.canonicalize()?;
        if canonical != self.canonical_path {
            return Err(Error::ExternalMutation);
        }
        verify_root_identity(&self.directory, &self.canonical_path)?;
        if held_directory_identity(&self.directory)? != self.identity
            || inspect_android_held_directory(&self.directory)? != self.acl
        {
            return Err(Error::ExternalMutation);
        }
        Ok(())
    }

    /// Clones the inspected held directory for a native-provenance adapter.
    ///
    /// # Errors
    /// Returns an I/O error if the descriptor can no longer be cloned.
    #[doc(hidden)]
    pub fn try_clone_directory(&self) -> Result<Dir, Error> {
        Ok(self.directory.try_clone()?)
    }

    /// Returns the canonical path retained during inspection.
    #[doc(hidden)]
    #[must_use]
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }
}

/// Applies mode 0700 to a held, newly created Android directory.
///
/// # Errors
/// Returns an I/O error when the held permission update fails.
#[cfg(target_os = "android")]
pub fn harden_android_new_directory(directory: &Dir) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    directory
        .try_clone()?
        .into_std_file()
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Inspects a held Android directory without granting native provenance.
///
/// # Errors
/// Rejects wrong type, owner, exact mode, or present ACL xattrs.
#[cfg(target_os = "android")]
pub fn inspect_android_held_directory(directory: &Dir) -> Result<AndroidAclInspection, Error> {
    use std::os::unix::fs::MetadataExt;
    let held = directory.try_clone()?.into_std_file();
    let metadata = held.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(Error::ExternalMutation);
    }
    inspect_android_acl(&held)
}

/// Applies mode 0600 to a held, newly created Android file.
///
/// # Errors
/// Returns an I/O error when the held permission update fails.
#[cfg(target_os = "android")]
pub fn harden_android_new_file(file: &File) -> Result<(), Error> {
    use cap_std::fs::{Permissions, PermissionsExt};
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(())
}

/// Inspects a held Android file without granting native provenance.
///
/// # Errors
/// Rejects wrong type, owner, exact mode, nlink other than one, or present ACL xattrs.
#[cfg(target_os = "android")]
pub fn inspect_android_held_file(file: &File) -> Result<AndroidAclInspection, Error> {
    inspect_android_held_file_links(file, 1)
}

/// Inspects a held Android private file with an exact bounded link count.
///
/// This is used only while atomically publishing a fully fsynced private stage
/// through a temporary second hard link. Ordinary private files require one
/// link through [`inspect_android_held_file`].
///
/// # Errors
/// Rejects wrong type, owner, exact mode/link count, or present ACL xattrs.
#[cfg(target_os = "android")]
pub fn inspect_android_held_file_links(
    file: &File,
    expected_links: u64,
) -> Result<AndroidAclInspection, Error> {
    use std::os::unix::fs::MetadataExt;
    if !(1..=2).contains(&expected_links) {
        return Err(Error::ExternalMutation);
    }
    let held = file.try_clone()?.into_std();
    let metadata = held.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != expected_links
    {
        return Err(Error::ExternalMutation);
    }
    inspect_android_acl(&held)
}

/// Creates or opens one private child below a native-proven Android no-backup
/// capability. Existing children are validated and never repaired.
///
/// # Errors
/// Rejects invalid names, links, wrong type/mode/owner, ACLs, and sync failures.
#[cfg(target_os = "android")]
pub fn create_or_open_android_private_dir(
    parent: &Dir,
    name: impl AsRef<Path>,
) -> Result<Dir, Error> {
    let name = name.as_ref();
    validate_child_name(name)?;
    let created = match parent.create_dir(name) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let directory = open_child_dir_nofollow(parent, name)?;
    if created {
        harden_android_new_directory(&directory)?;
    }
    inspect_android_held_directory(&directory)?;
    sync_directory(&directory)?;
    sync_directory(parent)?;
    Ok(directory)
}

/// Opens one existing private file below a native-proven Android no-backup
/// capability without following links.
///
/// # Errors
/// Rejects invalid names, links, wrong type/mode/owner, ACLs, or nlink != 1.
#[cfg(target_os = "android")]
pub fn open_android_private_file(parent: &Dir, name: impl AsRef<Path>) -> Result<File, Error> {
    let name = name.as_ref();
    validate_child_name(name)?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = parent.open_with(name, &options)?;
    inspect_android_held_file(&file)?;
    Ok(file)
}

/// Inspects a candidate Android private root without granting trust. Only a
/// native adapter that obtained the path from `getNoBackupFilesDir()` may turn
/// an `Unsupported` ACL result into a trusted no-backup capability.
///
/// # Errors
/// Fails for unstable, overlapping, symlinked, incorrectly owned, or non-0700 roots.
#[cfg(target_os = "android")]
pub fn inspect_android_private_root(
    candidate: &Path,
    other_root: &Path,
) -> Result<InspectedAndroidPrivateRoot, Error> {
    let inspected = inspect_android_no_backup_root(candidate)?;
    let other_before = other_root.canonicalize()?;
    let other = open_absolute_dir_nofollow(other_root)?;
    let other_after = other_root.canonicalize()?;
    if other_before != other_after {
        return Err(Error::InvalidRoot("root changed while it was opened"));
    }
    verify_root_identity(&other, &other_after)?;
    validate_disjoint_canonical(inspected.canonical_path(), &other_after)?;
    inspected.revalidate()?;
    Ok(inspected)
}

/// Inspects an Android private root without requiring an ambient Vault path.
/// This is the SAF-compatible validation lane.
///
/// The returned inspection does not itself prove `getNoBackupFilesDir()`
/// provenance. A native adapter must obtain the candidate directly from
/// Android and retain it inside an opaque native capability.
///
/// # Errors
/// Fails for unstable, symlinked, incorrectly owned, non-0700, or ACL-bearing roots.
#[cfg(target_os = "android")]
pub fn inspect_android_no_backup_root(
    candidate: &Path,
) -> Result<InspectedAndroidPrivateRoot, Error> {
    use std::os::unix::fs::MetadataExt;

    let candidate_before = candidate.canonicalize()?;
    let directory = open_android_native_dir_nofollow(candidate)?;
    let candidate_after = candidate.canonicalize()?;
    if candidate_before != candidate_after {
        return Err(Error::InvalidRoot("root changed while it was opened"));
    }
    verify_root_identity(&directory, &candidate_after)?;
    let held = directory.try_clone()?.into_std_file();
    let metadata = held.metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o777 != 0o700 {
        return Err(Error::InvalidRoot(
            "Android no-backup directory must be current-user owned with mode 0700",
        ));
    }
    let acl = inspect_android_acl(&held)?;
    let identity = held_directory_identity(&directory)?;
    Ok(InspectedAndroidPrivateRoot {
        directory,
        canonical_path: candidate_after,
        identity,
        acl,
    })
}

#[cfg(target_os = "android")]
fn open_android_native_dir_nofollow(candidate: &Path) -> Result<Dir, Error> {
    // Android app sandboxes permit traversing private parent directories but
    // deny opening those parents for enumeration. Open the exact native-proven
    // leaf directly, while rejecting a final symlink before and after open.
    if fs::symlink_metadata(candidate)?.file_type().is_symlink() {
        return Err(Error::InvalidRoot("root contains a symlink component"));
    }
    let directory = Dir::open_ambient_dir(candidate, ambient_authority())?;
    if fs::symlink_metadata(candidate)?.file_type().is_symlink() {
        return Err(Error::InvalidRoot("root contains a symlink component"));
    }
    Ok(directory)
}

#[cfg(target_os = "android")]
fn inspect_android_acl(file: &std::fs::File) -> Result<AndroidAclInspection, Error> {
    use xattr::FileExt;
    let mut unsupported = false;
    for name in ["system.posix_acl_access", "system.posix_acl_default"] {
        match file.get_xattr(name) {
            Ok(Some(_)) => return Err(Error::ExtendedAcl),
            Ok(None) => {}
            Err(error) if android_acl_query_unavailable(&error) => {
                unsupported = true;
            }
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(if unsupported {
        AndroidAclInspection::Unsupported
    } else {
        AndroidAclInspection::Clean
    })
}

#[cfg(any(target_os = "android", test))]
fn android_acl_query_unavailable(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
        || error.raw_os_error() == Some(rustix::io::Errno::NOTSUP.raw_os_error())
        || error.raw_os_error() == Some(rustix::io::Errno::ACCESS.raw_os_error())
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::DirectorySyncUnsupported(error) => {
                write!(formatter, "directory sync is unsupported: {error}")
            }
            Self::InvalidRoot(reason) => write!(formatter, "invalid private root: {reason}"),
            Self::PrivacyValidationRequired => {
                formatter.write_str("robust platform privacy validation is required")
            }
            Self::ExtendedAcl => {
                formatter.write_str("private filesystem object has an extended ACL")
            }
            Self::ExternalMutation => {
                formatter.write_str("private filesystem object was modified externally")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::DirectorySyncUnsupported(error) => Some(error),
            Self::InvalidRoot(_)
            | Self::PrivacyValidationRequired
            | Self::ExtendedAcl
            | Self::ExternalMutation => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Opens and validates a private app-data root that is disjoint from a second root.
/// Both roots are opened without following symlinks and checked against their
/// canonical identities before the returned app-data capability is trusted.
///
/// # Errors
///
/// Fails for I/O errors, invalid or overlapping roots, insecure permissions or
/// ACLs, and platforms without complete privacy validation.
pub fn open_private_disjoint_root(app_data_root: &Path, other_root: &Path) -> Result<Dir, Error> {
    open_private_disjoint_held_root(app_data_root, other_root)?.try_clone_directory()
}

/// Opens the standard desktop private/disjoint boundary while retaining the
/// canonical path and held directory identity for later revalidation.
///
/// # Errors
/// Fails for the same reasons as [`open_private_disjoint_root`].
pub fn open_private_disjoint_held_root(
    app_data_root: &Path,
    other_root: &Path,
) -> Result<HeldPrivateRoot, Error> {
    let canonical_path = app_data_root.canonicalize()?;
    let (directory, _) = open_validated_disjoint_roots(app_data_root, other_root)?;
    let identity = held_directory_identity(&directory)?;
    let root = HeldPrivateRoot {
        directory,
        canonical_path,
        identity,
    };
    root.revalidate()?;
    Ok(root)
}

/// Opens the same private/disjoint boundary as [`open_private_disjoint_root`]
/// while retaining both held roots and stable Unix device/inode binding facts.
///
/// This deliberately fails closed outside macOS and Linux. A caller that
/// persists Unix binding facts must not silently substitute a different
/// platform identity model.
///
/// # Errors
/// Fails for the same reasons as [`open_private_disjoint_root`], or when the
/// target cannot provide the exact Unix binding contract.
pub fn open_private_disjoint_roots_with_unix_identity(
    app_data_root: &Path,
    other_root: &Path,
) -> Result<PrivateDisjointRoots, Error> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (app_data_root, other_root);
        Err(Error::PrivacyValidationRequired)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::MetadataExt;

        let (app_directory, other_directory) =
            open_validated_disjoint_roots(app_data_root, other_root)?;
        let metadata = other_directory.try_clone()?.into_std_file().metadata()?;
        Ok(PrivateDisjointRoots {
            private_root: app_directory,
            other_root: other_directory,
            other_identity: UnixRootIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        })
    }
}

fn open_validated_disjoint_roots(
    app_data_root: &Path,
    other_root: &Path,
) -> Result<(Dir, Dir), Error> {
    let app_before = app_data_root.canonicalize()?;
    let other_before = other_root.canonicalize()?;
    let app_directory = open_absolute_dir_nofollow(app_data_root)?;
    let other_directory = open_absolute_dir_nofollow(other_root)?;
    let app_after = app_data_root.canonicalize()?;
    let other_after = other_root.canonicalize()?;
    if app_before != app_after || other_before != other_after {
        return Err(Error::InvalidRoot("root changed while it was opened"));
    }
    validate_disjoint_canonical(&app_after, &other_after)?;
    verify_root_identity(&app_directory, &app_after)?;
    verify_root_identity(&other_directory, &other_after)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let app_metadata = app_directory.try_clone()?.into_std_file().metadata()?;
        let other_metadata = other_directory.try_clone()?.into_std_file().metadata()?;
        if app_metadata.dev() == other_metadata.dev() && app_metadata.ino() == other_metadata.ino()
        {
            return Err(Error::InvalidRoot(
                "private and other roots resolve to the same held directory identity",
            ));
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if app_metadata.dev() == other_metadata.dev()
            && myvault_platform_fs::mount_identity(&app_directory).map_err(mount_proof_error)?
                != myvault_platform_fs::mount_identity(&other_directory)
                    .map_err(mount_proof_error)?
        {
            return Err(Error::InvalidRoot(
                "same-device roots belong to different mount instances",
            ));
        }
    }
    require_private_directory(&app_directory)?;
    Ok((app_directory, other_directory))
}

/// Creates a private child directory if absent, then opens and validates it
/// without following symlinks. The parent is synced after a successful create.
///
/// # Errors
///
/// Fails if `name` is not one normalized UTF-8 component, the child topology or
/// privacy is invalid, durability fails, or validation is unsupported.
pub fn create_or_open_private_dir(parent: &Dir, name: impl AsRef<Path>) -> Result<Dir, Error> {
    let name = name.as_ref();
    validate_child_name(name)?;
    let created = match parent.create_dir(name) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let directory = open_child_dir_nofollow(parent, name)?;
    if created {
        set_private_directory_permissions(&directory)?;
    }
    require_private_directory(&directory)?;
    sync_directory(&directory)?;
    sync_directory(parent)?;
    Ok(directory)
}

/// Creates a new private child directory and never reuses or repairs an
/// existing name. The parent is synced after the new directory is hardened.
///
/// # Errors
/// Returns [`io::ErrorKind::AlreadyExists`] through [`Error::Io`] for a name
/// collision, and otherwise fails closed on invalid topology or privacy.
pub fn create_private_dir(parent: &Dir, name: impl AsRef<Path>) -> Result<Dir, Error> {
    let name = name.as_ref();
    validate_child_name(name)?;
    parent.create_dir(name)?;
    let directory = open_child_dir_nofollow(parent, name)?;
    set_private_directory_permissions(&directory)?;
    require_private_directory(&directory)?;
    sync_directory(&directory)?;
    sync_directory(parent)?;
    Ok(directory)
}

/// Opens and validates an existing private child directory without following
/// symlinks and without creating or repairing anything.
///
/// # Errors
/// Fails if `name` is invalid, absent, symlinked, not a directory, or violates
/// the platform private-directory policy.
pub fn open_private_dir(parent: &Dir, name: impl AsRef<Path>) -> Result<Dir, Error> {
    let name = name.as_ref();
    validate_child_name(name)?;
    let directory = open_child_dir_nofollow(parent, name)?;
    require_private_directory(&directory)?;
    Ok(directory)
}

fn validate_child_name(name: &Path) -> Result<(), Error> {
    let mut components = name.components();
    let Some(Component::Normal(component)) = components.next() else {
        return Err(Error::InvalidRoot(
            "child name must be one normalized UTF-8 component",
        ));
    };
    if components.next().is_some() || component.to_str().is_none() {
        return Err(Error::InvalidRoot(
            "child name must be one normalized UTF-8 component",
        ));
    }
    Ok(())
}

/// Applies the private regular-file policy to an already-open, newly created
/// dedicated file. This must not be used to repair an arbitrary existing file.
///
/// # Errors
///
/// Fails when permissions cannot be set or the platform lacks a safe policy.
pub fn set_private_file_permissions(file: &File) -> Result<(), Error> {
    platform_set_private_file_permissions(file)
}

/// Verifies that an already-open private file is owned by the current user,
/// has exact private mode, has no extended ACL, and is within the link bound.
///
/// # Errors
///
/// Fails for I/O errors, external mutation, extended ACLs, or unsupported privacy validation.
pub fn verify_private_file(file: &File, max_links: u64) -> Result<(), Error> {
    platform_verify_private_file(file, max_links)
}

/// Durably syncs an already-open directory capability.
///
/// # Errors
///
/// Returns the operating-system error when the held directory cannot be synced.
pub fn sync_directory(directory: &Dir) -> Result<(), Error> {
    #[cfg(target_os = "android")]
    {
        use std::os::fd::AsFd;

        let reopened = rustix::fs::openat(
            directory.as_fd(),
            ".",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| Error::Io(io::Error::from_raw_os_error(error.raw_os_error())))?;
        rustix::fs::fsync(&reopened)
            .map_err(|error| Error::Io(io::Error::from_raw_os_error(error.raw_os_error())))?;
        return Ok(());
    }

    #[cfg(not(target_os = "android"))]
    match directory.try_clone()?.into_std_file().sync_all() {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput
                    | io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
            ) =>
        {
            Err(Error::DirectorySyncUnsupported(error))
        }
        Err(error) => Err(Error::Io(error)),
    }
}

fn validate_disjoint_canonical(app: &Path, other: &Path) -> Result<(), Error> {
    if app == other || app.starts_with(other) || other.starts_with(app) {
        return Err(Error::InvalidRoot(
            "app data and vault roots must be disjoint",
        ));
    }
    Ok(())
}

fn open_absolute_dir_nofollow(path: &Path) -> Result<Dir, Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidRoot("root must be absolute"));
    }
    #[cfg(windows)]
    validate_root_namespace(path)?;
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
    #[cfg(windows)]
    require_non_reparse_directory(&directory)?;
    for name in names {
        directory = open_child_dir_nofollow(&directory, &name)?;
    }
    Ok(directory)
}

#[cfg(windows)]
fn validate_root_namespace(path: &Path) -> Result<(), Error> {
    use std::path::Prefix;
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => {}
        _ => {
            return Err(Error::InvalidRoot(
                "device, verbatim, and network roots are unsupported",
            ));
        }
    }
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(Error::InvalidRoot("root must be drive absolute"));
    }
    Ok(())
}

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
    #[cfg(windows)]
    if !myvault_platform_acl::is_non_reparse_handle(
        &file.try_clone()?.into_std(),
        myvault_platform_acl::ObjectKind::Directory,
    )? {
        return Err(Error::InvalidRoot(
            "root contains a reparse point or device",
        ));
    }
    Ok(Dir::from_std_file(file.into_std()))
}

#[cfg(windows)]
fn require_non_reparse_directory(directory: &Dir) -> Result<(), Error> {
    let held = directory.try_clone()?.into_std_file();
    if !myvault_platform_acl::is_non_reparse_handle(
        &held,
        myvault_platform_acl::ObjectKind::Directory,
    )? {
        return Err(Error::InvalidRoot(
            "root contains a reparse point or device",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verify_root_identity(directory: &Dir, canonical: &Path) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;
    let held = directory.try_clone()?.into_std_file().metadata()?;
    let verification = open_absolute_dir_nofollow(canonical)?;
    let ambient = verification.try_clone()?.into_std_file().metadata()?;
    if held.dev() != ambient.dev() || held.ino() != ambient.ino() {
        return Err(Error::InvalidRoot(
            "root identity changed while it was opened",
        ));
    }
    let held_mount = myvault_platform_fs::mount_identity(directory).map_err(mount_proof_error)?;
    let ambient_mount =
        myvault_platform_fs::mount_identity(&verification).map_err(mount_proof_error)?;
    if held_mount != ambient_mount {
        return Err(Error::InvalidRoot(
            "root resolves through different mount instances",
        ));
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn mount_proof_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::Unsupported {
        Error::PrivacyValidationRequired
    } else {
        Error::Io(error)
    }
}

#[cfg(windows)]
fn verify_root_identity(directory: &Dir, canonical: &Path) -> Result<(), Error> {
    let held = myvault_platform_fs::directory_identity(directory)?;
    let ambient = Dir::open_ambient_dir(canonical, ambient_authority())?;
    require_non_reparse_directory(&ambient)?;
    if held != myvault_platform_fs::directory_identity(&ambient)? {
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
    verify_no_extended_acl(&held)
}

#[cfg(windows)]
fn require_private_directory(directory: &Dir) -> Result<(), Error> {
    let held = directory.try_clone()?.into_std_file();
    if !myvault_platform_acl::is_private_handle(
        &held,
        myvault_platform_acl::ObjectKind::Directory,
        1,
    )? {
        return Err(Error::InvalidRoot(
            "private directory owner, DACL, topology, or links are unsafe",
        ));
    }
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn require_private_directory(_directory: &Dir) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(unix)]
fn set_private_directory_permissions(directory: &Dir) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    directory
        .try_clone()?
        .into_std_file()
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
fn set_private_directory_permissions(directory: &Dir) -> Result<(), Error> {
    let held = directory.try_clone()?.into_std_file();
    myvault_platform_acl::harden_private_handle(
        &held,
        myvault_platform_acl::ObjectKind::Directory,
    )?;
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn set_private_directory_permissions(_directory: &Dir) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(unix)]
fn platform_set_private_file_permissions(file: &File) -> Result<(), Error> {
    use cap_std::fs::{Permissions, PermissionsExt};
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(windows)]
fn platform_set_private_file_permissions(file: &File) -> Result<(), Error> {
    let held = file.try_clone()?.into_std();
    myvault_platform_acl::harden_private_handle(&held, myvault_platform_acl::ObjectKind::File)?;
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn platform_set_private_file_permissions(_file: &File) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
}

#[cfg(unix)]
fn platform_verify_private_file(file: &File, max_links: u64) -> Result<(), Error> {
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
    verify_no_extended_acl(&held)
}

#[cfg(windows)]
fn platform_verify_private_file(file: &File, max_links: u64) -> Result<(), Error> {
    let held = file.try_clone()?.into_std();
    if !myvault_platform_acl::is_private_handle(
        &held,
        myvault_platform_acl::ObjectKind::File,
        max_links,
    )? {
        return Err(Error::ExternalMutation);
    }
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn platform_verify_private_file(_file: &File, _max_links: u64) -> Result<(), Error> {
    Err(Error::PrivacyValidationRequired)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn roots() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let base = temporary.path().canonicalize().expect("canonical root");
        let app = base.join("app");
        let other = base.join("vault");
        fs::create_dir(&app).expect("app root");
        fs::create_dir(&other).expect("other root");
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app root");
        (temporary, app, other)
    }

    #[test]
    fn android_acl_unavailable_accepts_eacces_but_not_eperm() {
        assert!(android_acl_query_unavailable(&io::Error::from(
            io::ErrorKind::Unsupported
        )));
        assert!(android_acl_query_unavailable(
            &io::Error::from_raw_os_error(rustix::io::Errno::ACCESS.raw_os_error())
        ));
        assert!(android_acl_query_unavailable(
            &io::Error::from_raw_os_error(rustix::io::Errno::NOTSUP.raw_os_error())
        ));
        assert!(!android_acl_query_unavailable(
            &io::Error::from_raw_os_error(rustix::io::Errno::PERM.raw_os_error())
        ));
        assert!(!android_acl_query_unavailable(&io::Error::from(
            io::ErrorKind::InvalidData
        )));
    }

    #[test]
    fn opens_disjoint_private_root_and_private_child() {
        let (_temporary, app, other) = roots();
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        let child = create_or_open_private_dir(&root, "journal").expect("private child");
        assert_eq!(
            child
                .into_std_file()
                .metadata()
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn rejects_overlapping_roots() {
        let (_temporary, app, _other) = roots();
        let nested = app.join("nested");
        fs::create_dir(&nested).expect("nested root");
        assert!(matches!(
            open_private_disjoint_root(&app, &nested),
            Err(Error::InvalidRoot(
                "app data and vault roots must be disjoint"
            ))
        ));
    }

    #[test]
    fn rejects_public_app_root_without_repairing_it() {
        let (_temporary, app, other) = roots();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o755)).expect("public mode");
        assert!(matches!(
            open_private_disjoint_root(&app, &other),
            Err(Error::InvalidRoot(_))
        ));
        assert_eq!(
            fs::metadata(app).expect("metadata").permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn rejects_symlink_component() {
        use std::os::unix::fs::symlink;
        let (temporary, app, other) = roots();
        let link = temporary.path().join("app-link");
        symlink(&app, &link).expect("symlink");
        assert!(matches!(
            open_private_disjoint_root(&link, &other),
            Err(Error::InvalidRoot("root contains a symlink component"))
        ));
    }

    #[test]
    fn rejects_symlink_child_without_repairing_target() {
        use std::os::unix::fs::symlink;
        let (temporary, app, other) = roots();
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        let target = temporary.path().join("target");
        fs::create_dir(&target).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("public target");
        symlink(&target, app.join("journal")).expect("child symlink");
        assert!(matches!(
            create_or_open_private_dir(&root, "journal"),
            Err(Error::InvalidRoot("root contains a symlink component"))
        ));
        assert_eq!(
            fs::metadata(target)
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn rejects_existing_public_child_without_repairing_it() {
        let (_temporary, app, other) = roots();
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        let child = app.join("journal");
        fs::create_dir(&child).expect("child");
        fs::set_permissions(&child, fs::Permissions::from_mode(0o755)).expect("public child");
        assert!(matches!(
            create_or_open_private_dir(&root, "journal"),
            Err(Error::InvalidRoot(
                "private directory grants group or world access"
            ))
        ));
        assert_eq!(
            fs::metadata(child)
                .expect("child metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn file_mode_and_link_count_are_enforced() {
        let (_temporary, app, other) = roots();
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let file = root.open_with("record", &options).expect("record");
        set_private_file_permissions(&file).expect("private file mode");
        verify_private_file(&file, 1).expect("private file");
        root.hard_link("record", &root, "alias").expect("hard link");
        assert!(matches!(
            verify_private_file(&file, 1),
            Err(Error::ExternalMutation)
        ));
    }

    #[test]
    fn child_name_cannot_escape_or_add_path_components() {
        let (_temporary, app, other) = roots();
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        for invalid in ["", ".", "..", "../escape", "nested/child", "/absolute"] {
            assert!(matches!(
                create_or_open_private_dir(&root, invalid),
                Err(Error::InvalidRoot(
                    "child name must be one normalized UTF-8 component"
                ))
            ));
        }
        assert!(!app.join("escape").exists());
        assert!(!app.join("nested").exists());
    }

    #[test]
    fn held_pair_reports_the_other_root_identity() {
        use std::os::unix::fs::MetadataExt;

        let (_temporary, app, other) = roots();
        let roots =
            open_private_disjoint_roots_with_unix_identity(&app, &other).expect("held root pair");
        let expected = roots
            .other_root()
            .try_clone()
            .expect("clone other")
            .into_std_file()
            .metadata()
            .expect("other metadata");
        assert_eq!(roots.other_identity().device(), expected.dev());
        assert_eq!(roots.other_identity().inode(), expected.ino());
    }

    #[test]
    fn create_private_dir_never_reuses_a_collision() {
        let (_temporary, app, other) = roots();
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        create_private_dir(&root, "work").expect("new private directory");
        let error = create_private_dir(&root, "work").expect_err("collision");
        assert!(matches!(error, Error::Io(error) if error.kind() == io::ErrorKind::AlreadyExists));
    }

    #[test]
    fn identity_checked_one_component_removal_rejects_replacement_and_nonempty_directory() {
        let (_temporary, app, other) = roots();
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let file = root.open_with("record", &options).expect("record");
        set_private_file_permissions(&file).expect("private record");
        let identity = held_private_file_identity(&file).expect("identity");
        remove_private_file_if_identity(&root, "record", &file, &identity)
            .expect("remove exact file");
        assert!(matches!(
            remove_private_file_if_identity(&root, "record", &file, &identity),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound
        ));

        let child = create_private_dir(&root, "child").expect("child");
        let child_identity = held_directory_identity(&child).expect("child identity");
        child.create("nested").expect("nested");
        assert!(matches!(
            remove_empty_private_dir_if_identity(&root, "child", &child, &child_identity),
            Err(Error::ExternalMutation)
        ));
    }

    #[test]
    fn identity_checked_removal_binds_token_to_original_live_handle() {
        let (_temporary, app, other) = roots();
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let original = root.open_with("record", &options).expect("original");
        set_private_file_permissions(&original).expect("private original");
        let original_identity = held_private_file_identity(&original).expect("original identity");
        root.rename("record", &root, "displaced")
            .expect("displace original");
        let replacement = root.open_with("record", &options).expect("replacement");
        set_private_file_permissions(&replacement).expect("private replacement");

        assert!(matches!(
            remove_private_file_if_identity(&root, "record", &original, &original_identity),
            Err(Error::ExternalMutation)
        ));
        assert!(root.open("record").is_ok());

        let replacement_identity =
            held_private_file_identity(&replacement).expect("replacement identity");
        assert!(matches!(
            remove_private_file_if_identity(&root, "record", &original, &replacement_identity),
            Err(Error::ExternalMutation)
        ));
        assert!(root.open("record").is_ok());
    }

    #[test]
    fn identity_checked_empty_directory_removal_rejects_named_substitution() {
        let (_temporary, app, other) = roots();
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        let original = create_private_dir(&root, "child").expect("original child");
        let identity = held_directory_identity(&original).expect("original identity");
        root.rename("child", &root, "displaced")
            .expect("displace original");
        create_private_dir(&root, "child").expect("replacement child");

        assert!(matches!(
            remove_empty_private_dir_if_identity(&root, "child", &original, &identity),
            Err(Error::ExternalMutation)
        ));
        assert!(open_private_dir(&root, "child").is_ok());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn harden_directory(path: &Path) {
        let directory = Dir::open_ambient_dir(path, ambient_authority()).expect("open directory");
        myvault_platform_acl::harden_private_handle(
            &directory.into_std_file(),
            myvault_platform_acl::ObjectKind::Directory,
        )
        .expect("harden directory");
    }

    #[test]
    fn arbitrary_root_is_not_repaired_but_explicitly_provisioned_root_works() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let app = temporary.path().join("app");
        let other = temporary.path().join("vault");
        std::fs::create_dir(&app).expect("app");
        std::fs::create_dir(&other).expect("vault");
        assert!(matches!(
            open_private_disjoint_root(&app, &other),
            Err(Error::InvalidRoot(_))
        ));
        harden_directory(&app);
        let root = open_private_disjoint_root(&app, &other).expect("private root");
        assert!(matches!(
            create_or_open_private_dir(&root, "journal"),
            Err(Error::DirectorySyncUnsupported(_))
        ));
    }
}
