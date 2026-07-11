use std::ffi::{c_void, OsStr};
use std::io;
use std::mem::{align_of, offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::ptr;

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use windows_sys::Wdk::Storage::FileSystem::{
    FileRenameInformationEx, NtSetInformationFile, RtlNtStatusToDosErrorNoTeb,
    FILE_RENAME_INFORMATION, FILE_RENAME_INFORMATION_0,
};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, HANDLE, NTSTATUS, STATUS_INVALID_DEVICE_REQUEST,
    STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_EXISTS,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileAttributeTagInfo, GetFileInformationByHandleEx, DELETE, FILE_ATTRIBUTE_DEVICE,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    INVALID_FILE_ATTRIBUTES,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

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
    let source_handle = source.as_raw_handle() as HANDLE;
    validate_source_handle(source_handle)?;

    let destination_utf16 = destination_name.encode_wide().collect::<Vec<_>>();
    let mut buffer = RenameBuffer::new(
        destination_parent.as_raw_handle() as HANDLE,
        &destination_utf16,
    )?;
    let mut io_status = IO_STATUS_BLOCK::default();

    // SAFETY: the source and destination-directory handles are held by safe
    // wrappers for the duration of this synchronous call. `RenameBuffer`
    // owns an aligned, checked-length FILE_RENAME_INFORMATION followed by the
    // exact counted UTF-16 name. Flags=0 is the no-replace contract. No ambient
    // destination path or check-then-rename fallback is used.
    let status = unsafe {
        NtSetInformationFile(
            source_handle,
            &raw mut io_status,
            buffer.as_mut_ptr().cast_const().cast::<c_void>(),
            buffer.byte_len,
            FileRenameInformationEx,
        )
    };
    map_ntstatus(status)
}

fn validate_source_handle(handle: HANDLE) -> io::Result<()> {
    let mut information = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `information` is a live, correctly sized output object and the
    // handle is owned by the caller for the duration of this synchronous call.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&raw mut information).cast::<c_void>(),
            u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).expect("Win32 struct fits u32"),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    if information.FileAttributes == INVALID_FILE_ATTRIBUTES
        || information.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE) != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic rename source must be a non-reparse file or directory",
        ));
    }
    Ok(())
}

fn map_ntstatus(status: NTSTATUS) -> io::Result<()> {
    if matches!(
        status,
        STATUS_OBJECT_NAME_COLLISION | STATUS_OBJECT_NAME_EXISTS
    ) {
        return Err(already_exists_error());
    }
    if status >= 0 {
        return Ok(());
    }
    if matches!(
        status,
        STATUS_NOT_SUPPORTED | STATUS_NOT_IMPLEMENTED | STATUS_INVALID_DEVICE_REQUEST
    ) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "FileRenameInformationEx is unsupported by this filesystem",
        ));
    }

    // SAFETY: this pure ntdll conversion accepts every NTSTATUS value and does
    // not dereference pointers or depend on thread-local last-error state.
    let dos_error = unsafe { RtlNtStatusToDosErrorNoTeb(status) };
    if matches!(dos_error, ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS) {
        Err(already_exists_error())
    } else {
        Err(io::Error::from_raw_os_error(i32::from_ne_bytes(
            dos_error.to_ne_bytes(),
        )))
    }
}

fn already_exists_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "atomic rename destination already exists",
    )
}

struct RenameBuffer {
    storage: Vec<usize>,
    byte_len: u32,
}

impl RenameBuffer {
    fn new(root_directory: HANDLE, filename: &[u16]) -> io::Result<Self> {
        if filename.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rename name must not be empty",
            ));
        }
        if align_of::<usize>() < align_of::<FILE_RENAME_INFORMATION>() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native rename buffer alignment is unavailable",
            ));
        }
        let filename_bytes = filename
            .len()
            .checked_mul(size_of::<u16>())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "rename name is too long")
            })?;
        let filename_bytes_u32 = u32::try_from(filename_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rename name is too long"))?;
        let buffer_bytes = offset_of!(FILE_RENAME_INFORMATION, FileName)
            .checked_add(filename_bytes)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "rename buffer is too large")
            })?;
        let byte_len = u32::try_from(buffer_bytes).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "rename buffer is too large")
        })?;
        let words = buffer_bytes
            .checked_add(size_of::<usize>() - 1)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "rename buffer is too large")
            })?
            / size_of::<usize>();
        let mut storage = vec![0_usize; words];
        let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();

        // SAFETY: the usize vector satisfies the checked alignment and storage
        // length above. The variable filename begins at the documented field
        // offset and exactly `filename_bytes` bytes fit in the allocation.
        unsafe {
            ptr::write(
                information,
                FILE_RENAME_INFORMATION {
                    Anonymous: FILE_RENAME_INFORMATION_0 { Flags: 0 },
                    RootDirectory: root_directory,
                    FileNameLength: filename_bytes_u32,
                    FileName: [0],
                },
            );
            ptr::copy_nonoverlapping(
                filename.as_ptr(),
                ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
                filename.len(),
            );
        }
        Ok(Self { storage, byte_len })
    }

    fn as_mut_ptr(&mut self) -> *mut FILE_RENAME_INFORMATION {
        self.storage.as_mut_ptr().cast()
    }
}

fn validate_entry_name(name: &OsStr) -> io::Result<()> {
    let wide = name.encode_wide().collect::<Vec<_>>();
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

#[cfg(test)]
mod tests {
    use super::{map_ntstatus, RenameBuffer, FILE_RENAME_INFORMATION};
    use std::ffi::c_void;
    use std::mem::offset_of;
    use std::ptr;

    #[test]
    fn counted_string_buffer_handles_one_and_two_units() {
        assert_buffer(&[u16::from(b'x')]);
        assert_buffer(&[u16::from(b'a'), u16::from(b'b')]);
    }

    #[test]
    fn counted_string_buffer_handles_surrogate_pair_and_thai() {
        assert_buffer(&[0xD83D, 0xDE00]);
        assert_buffer(&"บันทึก".encode_utf16().collect::<Vec<_>>());
    }

    #[test]
    fn ntstatus_mapping_is_narrow_and_preserves_collision() {
        use windows_sys::Win32::Foundation::{
            STATUS_INVALID_PARAMETER, STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_COLLISION,
            STATUS_OBJECT_NAME_EXISTS,
        };

        assert_eq!(
            map_ntstatus(STATUS_OBJECT_NAME_COLLISION)
                .expect_err("collision")
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            map_ntstatus(STATUS_OBJECT_NAME_EXISTS)
                .expect_err("existing name")
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            map_ntstatus(STATUS_NOT_SUPPORTED)
                .expect_err("unsupported")
                .kind(),
            std::io::ErrorKind::Unsupported
        );
        assert_ne!(
            map_ntstatus(STATUS_INVALID_PARAMETER)
                .expect_err("invalid parameter")
                .kind(),
            std::io::ErrorKind::Unsupported
        );
    }

    fn assert_buffer(expected: &[u16]) {
        let root = ptr::dangling_mut::<c_void>();
        let mut buffer = RenameBuffer::new(root, expected).expect("rename buffer");
        let information = buffer.as_mut_ptr();
        assert_eq!(
            buffer.byte_len as usize,
            offset_of!(FILE_RENAME_INFORMATION, FileName) + size_of_val(expected)
        );
        // SAFETY: `buffer` owns a fully initialized FILE_RENAME_INFORMATION
        // and the checked counted string remains live for these reads.
        unsafe {
            assert_eq!((*information).RootDirectory, root);
            assert_eq!(
                (*information).FileNameLength as usize,
                size_of_val(expected)
            );
            assert_eq!((*information).Anonymous.Flags, 0);
            let actual = std::slice::from_raw_parts(
                ptr::addr_of!((*information).FileName).cast::<u16>(),
                expected.len(),
            );
            assert_eq!(actual, expected);
        }
    }
}
