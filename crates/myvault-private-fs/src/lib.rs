#![forbid(unsafe_code)]

//! Held-capability helpers for private application data.
//!
//! Supported Unix hosts validate ownership, modes, links, and extended ACLs.
//! Other targets fail closed until an equivalent privacy proof exists.

#[cfg(not(windows))]
use cap_fs_ext::OpenOptionsMaybeDirExt;
#[cfg(not(windows))]
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
#[cfg(not(windows))]
use cap_std::ambient_authority;
#[cfg(not(windows))]
use cap_std::fs::OpenOptions;
use cap_std::fs::{Dir, File};
use std::fmt;
#[cfg(unix)]
use std::fs;
use std::io;
#[cfg(not(windows))]
use std::path::Component;
use std::path::Path;
#[cfg(not(windows))]
use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidRoot(&'static str),
    PrivacyValidationRequired,
    ExtendedAcl,
    ExternalMutation,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
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
            Self::Io(error) => Some(error),
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
    #[cfg(windows)]
    {
        let _ = (app_data_root, other_root);
        Err(Error::PrivacyValidationRequired)
    }

    #[cfg(not(windows))]
    {
        let app_before = app_data_root.canonicalize()?;
        let other_before = other_root.canonicalize()?;
        let app_directory = open_absolute_dir_nofollow(app_data_root)?;
        let other_directory = open_absolute_dir_nofollow(other_root)?;
        let app_after = app_data_root.canonicalize()?;
        let other_after = other_root.canonicalize()?;
        if app_before != app_after || other_before != other_after {
            return Err(Error::InvalidRoot("root changed while it was opened"));
        }
        verify_root_identity(&app_directory, &app_after)?;
        verify_root_identity(&other_directory, &other_after)?;
        validate_disjoint_canonical(&app_after, &other_after)?;
        require_private_directory(&app_directory)?;
        Ok(app_directory)
    }
}

/// Creates a private child directory if absent, then opens and validates it
/// without following symlinks. The parent is synced after a successful create.
///
/// # Errors
///
/// Fails if `name` is not one normalized UTF-8 component, the child topology or
/// privacy is invalid, durability fails, or validation is unsupported.
pub fn create_or_open_private_dir(parent: &Dir, name: impl AsRef<Path>) -> Result<Dir, Error> {
    #[cfg(windows)]
    {
        let _ = (parent, name);
        Err(Error::PrivacyValidationRequired)
    }

    #[cfg(not(windows))]
    {
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
            sync_directory(parent)?;
        }
        require_private_directory(&directory)?;
        Ok(directory)
    }
}

#[cfg(not(windows))]
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

/// Applies the private regular-file mode to an already-open file.
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
    directory.try_clone()?.into_std_file().sync_all()?;
    Ok(())
}

#[cfg(not(windows))]
fn validate_disjoint_canonical(app: &Path, other: &Path) -> Result<(), Error> {
    if app == other || app.starts_with(other) || other.starts_with(app) {
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
    verify_no_extended_acl(&held)
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

#[cfg(not(unix))]
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

#[cfg(not(unix))]
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
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn root_privacy_fails_closed() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let app = temporary.path().join("app");
        let other = temporary.path().join("vault");
        std::fs::create_dir(&app).expect("app");
        std::fs::create_dir(&other).expect("vault");
        assert!(matches!(
            open_private_disjoint_root(&app, &other),
            Err(Error::PrivacyValidationRequired)
        ));
    }
}
