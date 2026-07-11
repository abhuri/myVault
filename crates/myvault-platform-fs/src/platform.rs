use std::ffi::OsStr;
use std::io;
use std::mem::{align_of, offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::ptr;

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
    ERROR_NOT_SUPPORTED, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileRenameInfoEx, SetFileInformationByHandle, DELETE, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};

pub(super) fn rename_noreplace(
    source_parent: &Dir,
    source_name: &OsStr,
    destination_parent: &Dir,
    destination_name: &OsStr,
) -> io::Result<()> {
    validate_entry_name(source_name)?;
    validate_entry_name(destination_name)?;
    let mut options = OpenOptions::new();
    options
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .follow(FollowSymlinks::No);
    let source = source_parent.open_with(source_name, &options)?;
    let destination_utf16: Vec<u16> = destination_name.encode_wide().collect();
    let filename_bytes = destination_utf16
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename name is too long"))?;
    let buffer_bytes = offset_of!(FILE_RENAME_INFO, FileName)
        .checked_add(filename_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer is too large"))?;
    let words = buffer_bytes.div_ceil(size_of::<usize>());
    let mut storage = vec![0_usize; words];
    debug_assert!(align_of::<usize>() >= align_of::<FILE_RENAME_INFO>());
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();

    // SAFETY: `storage` is aligned for FILE_RENAME_INFO and sized for its
    // variable-length UTF-16 filename. Both handles are owned by live safe
    // wrappers for the duration of the call. Flags=0 deliberately omits
    // FILE_RENAME_FLAG_REPLACE_IF_EXISTS, so Windows must fail if a destination
    // already exists. RootDirectory makes the destination descriptor-relative.
    let succeeded = unsafe {
        (*info).Anonymous.Flags = 0;
        (*info).RootDirectory = destination_parent.as_raw_handle() as HANDLE;
        (*info).FileNameLength = u32::try_from(filename_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rename name is too long"))?;
        ptr::copy_nonoverlapping(
            destination_utf16.as_ptr(),
            (*info).FileName.as_mut_ptr(),
            destination_utf16.len(),
        );
        SetFileInformationByHandle(
            source.as_raw_handle() as HANDLE,
            FileRenameInfoEx,
            info.cast(),
            u32::try_from(buffer_bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "rename buffer is too large")
            })?,
        )
    };
    if succeeded != 0 {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    match error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
    {
        Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "atomic rename destination already exists",
        )),
        Some(ERROR_INVALID_FUNCTION | ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED) => {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "FileRenameInfoEx is unsupported by this filesystem",
            ))
        }
        _ => Err(error),
    }
}

fn validate_entry_name(name: &OsStr) -> io::Result<()> {
    let wide: Vec<u16> = name.encode_wide().collect();
    if wide.is_empty()
        || wide == [u16::from(b'.')]
        || wide == [u16::from(b'.'), u16::from(b'.')]
        || wide.iter().any(|unit| matches!(*unit, 0 | 47 | 92))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "rename names must be single non-special path components",
        ));
    }
    Ok(())
}
