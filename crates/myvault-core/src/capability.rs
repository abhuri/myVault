use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};

use crate::{CoreError, Result};

/// Opens an absolute directory one component at a time without following links.
///
/// The returned handle is the authority used for all later relative operations;
/// callers must not reconstruct ambient paths after this boundary.
pub(crate) fn open_absolute_dir_nofollow(path: &Path) -> Result<Dir> {
    if !path.is_absolute() {
        return Err(CoreError::InvalidRelativePath(path.to_path_buf()));
    }

    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
            Component::RootDir => anchor.push(std::path::MAIN_SEPARATOR_STR),
            Component::Normal(name) => names.push(name.to_owned()),
            Component::CurDir | Component::ParentDir => {
                return Err(CoreError::InvalidRelativePath(path.to_path_buf()));
            }
        }
    }

    let mut current = Dir::open_ambient_dir(&anchor, ambient_authority())?;
    let mut traversed = anchor;
    for name in names {
        traversed.push(&name);
        current = open_child_dir_nofollow(&current, &name, &traversed)?;
    }
    Ok(current)
}

pub(crate) fn open_child_dir_nofollow(
    parent: &Dir,
    name: &std::ffi::OsStr,
    display_path: &Path,
) -> Result<Dir> {
    if parent
        .symlink_metadata(name)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(CoreError::SymlinkRejected(display_path.to_path_buf()));
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
            CoreError::SymlinkRejected(display_path.to_path_buf())
        } else {
            error.into()
        }
    })?;
    if !file.metadata()?.is_dir() {
        return Err(CoreError::InvalidRelativePath(display_path.to_path_buf()));
    }
    Ok(Dir::from_std_file(file.into_std()))
}
