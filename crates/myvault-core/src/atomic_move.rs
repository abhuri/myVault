use std::ffi::OsStr;
use std::io;

use cap_std::fs::Dir;

/// Renames one entry without replacing an existing destination.
///
/// Both names are resolved relative to already-open parent capabilities. The
/// platform implementation must provide no-replace atomically; an unavailable
/// primitive fails closed without falling back to a check-then-rename sequence.
pub(crate) fn rename_noreplace(
    source_parent: &Dir,
    source_name: &OsStr,
    destination_parent: &Dir,
    destination_name: &OsStr,
) -> io::Result<()> {
    rename_noreplace_platform(
        source_parent,
        source_name,
        destination_parent,
        destination_name,
    )
}

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
fn rename_noreplace_platform(
    source_parent: &Dir,
    source_name: &OsStr,
    destination_parent: &Dir,
    destination_name: &OsStr,
) -> io::Result<()> {
    rustix::fs::renameat_with(
        source_parent,
        source_name,
        destination_parent,
        destination_name,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(Into::into)
}

#[cfg(windows)]
fn rename_noreplace_platform(
    source_parent: &Dir,
    source_name: &OsStr,
    destination_parent: &Dir,
    destination_name: &OsStr,
) -> io::Result<()> {
    myvault_platform_fs::rename_noreplace(
        source_parent,
        source_name,
        destination_parent,
        destination_name,
    )
}

#[cfg(not(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    windows
)))]
fn rename_noreplace_platform(
    _source_parent: &Dir,
    _source_name: &OsStr,
    _destination_parent: &Dir,
    _destination_name: &OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform has no descriptor-relative atomic no-replace rename",
    ))
}
