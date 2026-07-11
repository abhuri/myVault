//! Safe, descriptor-relative ACL inspection for platforms whose standard
//! library does not expose the required query.

#![cfg(target_os = "macos")]

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;

unsafe extern "C" {
    fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> *mut c_void;
    fn acl_free(object: *mut c_void) -> c_int;
}

struct OwnedAcl(*mut c_void);

impl Drop for OwnedAcl {
    fn drop(&mut self) {
        // SAFETY: `OwnedAcl` is constructed only from a non-null pointer
        // returned by `acl_get_fd_np`, and ownership is released exactly once.
        let _ = unsafe { acl_free(self.0) };
    }
}

/// Returns whether the held file or directory descriptor has an extended ACL.
///
/// # Errors
/// Returns the platform error unless absence is reported with `ENOENT`, which
/// means that no extended ACL is attached.
pub fn has_extended_acl(file: &File) -> io::Result<bool> {
    unsafe extern "C" {
        fn __error() -> *mut c_int;
    }
    // SAFETY: the descriptor is borrowed for the duration of the call, and the
    // returned allocation is immediately wrapped for exactly-once release. The
    // Darwin errno cell belongs to this thread and is cleared before the call.
    let acl = unsafe {
        *__error() = 0;
        acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED)
    };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc_enoent()) {
            return Ok(false);
        }
        return Err(error);
    }
    let _owned = OwnedAcl(acl);
    Ok(true)
}

const fn libc_enoent() -> c_int {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_temporary_file_has_no_extended_acl() {
        let file = tempfile::tempfile().expect("temporary file");
        assert!(!has_extended_acl(&file).expect("inspect ACL"));
    }
}
