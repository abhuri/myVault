//! Safe, held-handle ACL inspection and hardening facade.

#[cfg(target_os = "macos")]
mod macos {
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
            // SAFETY: constructed only from an owned non-null ACL pointer.
            let _ = unsafe { acl_free(self.0) };
        }
    }

    /// Reports whether a held Darwin descriptor has an extended ACL.
    ///
    /// # Errors
    /// Returns the platform query error other than the no-ACL sentinel.
    pub fn has_extended_acl(file: &File) -> io::Result<bool> {
        unsafe extern "C" {
            fn __error() -> *mut c_int;
        }
        // SAFETY: the borrowed descriptor remains live; returned ownership is wrapped.
        let acl = unsafe {
            *__error() = 0;
            acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED)
        };
        if acl.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(2) {
                return Ok(false);
            }
            return Err(error);
        }
        let _owned = OwnedAcl(acl);
        Ok(true)
    }
}

#[cfg(target_os = "macos")]
pub use macos::has_extended_acl;

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::mem::{offset_of, size_of};
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, LocalFree, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, HLOCAL,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        AddAccessAllowedAceEx, CreateWellKnownSid, EqualSid, GetAce, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, InitializeAcl,
        IsValidSid, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE,
        ACCESS_DENIED_ACE, ACL, ACL_REVISION, ACL_REVISION_DS, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, INHERITED_ACE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
        TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FileAttributeTagInfo, FileStandardInfo, GetFileInformationByHandleEx, ReOpenFile,
        FILE_ALL_ACCESS, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
        INVALID_FILE_ATTRIBUTES, READ_CONTROL, WRITE_DAC,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ObjectKind {
        Directory,
        File,
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper exclusively owns a successful token handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    struct OwnedDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for OwnedDescriptor {
        fn drop(&mut self) {
            // SAFETY: GetSecurityInfo allocates this descriptor with LocalAlloc.
            let _ = unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }

    struct SidBuffer(Vec<usize>);

    impl SidBuffer {
        fn with_bytes(bytes: usize) -> io::Result<Self> {
            let words = bytes
                .checked_add(size_of::<usize>() - 1)
                .ok_or_else(|| io::Error::other("SID buffer size overflow"))?
                / size_of::<usize>();
            Ok(Self(vec![0; words]))
        }

        fn as_sid(&self) -> PSID {
            self.0.as_ptr().cast_mut().cast::<c_void>()
        }

        fn as_mut_sid(&mut self) -> PSID {
            self.0.as_mut_ptr().cast::<c_void>()
        }
    }

    /// Validates held-handle object kind and rejects reparse points and devices.
    ///
    /// # Errors
    /// Returns a Win32 query error.
    pub fn is_non_reparse_handle(file: &File, kind: ObjectKind) -> io::Result<bool> {
        let handle = file.as_raw_handle() as HANDLE;
        let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
        // SAFETY: live output structure and borrowed synchronous handle.
        if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileAttributeTagInfo,
                (&raw mut attributes).cast::<c_void>(),
                u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).map_err(size_error)?,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if attributes.FileAttributes == INVALID_FILE_ATTRIBUTES
            || attributes.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE)
                != 0
        {
            return Ok(false);
        }
        let standard = standard_info(handle)?;
        Ok(standard.Directory == matches!(kind, ObjectKind::Directory))
    }

    /// Validates owner, protected DACL, ACE allowlist, topology, and link count.
    ///
    /// # Errors
    /// Returns a Win32 security or metadata query error.
    pub fn is_private_handle(file: &File, kind: ObjectKind, max_links: u64) -> io::Result<bool> {
        if !is_non_reparse_handle(file, kind)? {
            return Ok(false);
        }
        let standard = standard_info(file.as_raw_handle() as HANDLE)?;
        if !(1..=max_links).contains(&u64::from(standard.NumberOfLinks)) {
            return Ok(false);
        }
        let current_user = current_user_sid()?;
        let system = well_known_sid(WinLocalSystemSid)?;
        let administrators = well_known_sid(WinBuiltinAdministratorsSid)?;
        let descriptor = security_descriptor(file)?;
        // SAFETY: descriptor remains owned and all output pointers are live.
        unsafe {
            validate_descriptor(
                descriptor.0,
                current_user.as_sid(),
                system.as_sid(),
                administrators.as_sid(),
                kind == ObjectKind::Directory,
            )
        }
    }

    /// Installs a protected current-user/SYSTEM/Administrators DACL on a held
    /// newly created dedicated object.
    ///
    /// Callers must not use this primitive to repair an existing arbitrary root.
    ///
    /// # Errors
    /// Returns a Win32 topology, handle-reopen, or security update error.
    pub fn harden_private_handle(file: &File, kind: ObjectKind) -> io::Result<()> {
        if !is_non_reparse_handle(file, kind)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private object must not be a reparse point or device",
            ));
        }
        let current_user = current_user_sid()?;
        let system = well_known_sid(WinLocalSystemSid)?;
        let administrators = well_known_sid(WinBuiltinAdministratorsSid)?;
        let sids = [
            current_user.as_sid(),
            system.as_sid(),
            administrators.as_sid(),
        ];
        let sid_lengths =
            sids.map(|sid| unsafe { windows_sys::Win32::Security::GetLengthSid(sid) } as usize);
        let ace_base = offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        let bytes = sid_lengths
            .iter()
            .try_fold(size_of::<ACL>(), |total, length| {
                total.checked_add(ace_base.checked_add(*length)?)
            });
        let bytes = bytes.ok_or_else(|| io::Error::other("ACL size overflow"))?;
        let mut acl_storage = SidBuffer::with_bytes(bytes)?;
        let acl = acl_storage.as_mut_sid().cast::<ACL>();
        // SAFETY: aligned storage is at least `bytes` and all SIDs remain live.
        if unsafe { InitializeAcl(acl, u32::try_from(bytes).map_err(size_error)?, ACL_REVISION) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let flags = if kind == ObjectKind::Directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        for sid in sids {
            // SAFETY: initialized ACL capacity includes all three exact ACE sizes.
            if unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, flags, FILE_ALL_ACCESS, sid) } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        // Reopen the same kernel object for WRITE_DAC. This never consults an
        // ambient path and permits callers to pass ordinary read/write handles.
        // SAFETY: the original handle remains live for this synchronous call.
        let security_handle = unsafe {
            ReOpenFile(
                file.as_raw_handle() as HANDLE,
                READ_CONTROL | WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            )
        };
        if security_handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let security_handle = OwnedHandle(security_handle);
        // SAFETY: held handle and initialized ACL stay live for the synchronous call.
        let status = unsafe {
            SetSecurityInfo(
                security_handle.0,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(win32_error_code(status)));
        }
        Ok(())
    }

    fn size_error(_: std::num::TryFromIntError) -> io::Error {
        io::Error::other("native security structure is too large")
    }

    fn win32_error_code(code: u32) -> i32 {
        i32::from_ne_bytes(code.to_ne_bytes())
    }

    fn standard_info(handle: HANDLE) -> io::Result<FILE_STANDARD_INFO> {
        let mut information = FILE_STANDARD_INFO::default();
        // SAFETY: live output structure and borrowed synchronous handle.
        if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                (&raw mut information).cast::<c_void>(),
                u32::try_from(size_of::<FILE_STANDARD_INFO>()).map_err(size_error)?,
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(information)
        }
    }

    fn current_user_sid() -> io::Result<SidBuffer> {
        let mut token = ptr::null_mut();
        // SAFETY: output pointer is live and process pseudo-handle is valid.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        let mut required = 0;
        // SAFETY: documented sizing query uses a null output buffer.
        let first = unsafe {
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &raw mut required)
        };
        if first != 0
            || io::Error::last_os_error().raw_os_error()
                != Some(win32_error_code(ERROR_INSUFFICIENT_BUFFER))
        {
            return Err(io::Error::last_os_error());
        }
        let mut token_storage = SidBuffer::with_bytes(required as usize)?;
        // SAFETY: storage has the exact size returned by the sizing query.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                token_storage.as_mut_sid(),
                required,
                &raw mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful TokenUser query initializes TOKEN_USER and its SID.
        let token_user = unsafe { &*token_storage.as_sid().cast::<TOKEN_USER>() };
        clone_sid(token_user.User.Sid)
    }

    fn clone_sid(sid: PSID) -> io::Result<SidBuffer> {
        // SAFETY: SID originates from a successful Win32 security query.
        let length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid) };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut copy = SidBuffer::with_bytes(length as usize)?;
        // SAFETY: both pointers are valid for exactly `length` bytes and do not overlap.
        unsafe {
            ptr::copy_nonoverlapping(
                sid.cast::<u8>(),
                copy.as_mut_sid().cast::<u8>(),
                length as usize,
            );
        }
        Ok(copy)
    }

    fn well_known_sid(kind: i32) -> io::Result<SidBuffer> {
        let mut required = 68_u32;
        let mut sid = SidBuffer::with_bytes(required as usize)?;
        // SAFETY: buffer is writable and domain SID is intentionally null.
        if unsafe { CreateWellKnownSid(kind, ptr::null_mut(), sid.as_mut_sid(), &raw mut required) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(sid)
    }

    fn security_descriptor(file: &File) -> io::Result<OwnedDescriptor> {
        let mut descriptor = ptr::null_mut();
        // SAFETY: only the self-relative descriptor output is requested and owned by caller.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &raw mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(win32_error_code(status)));
        }
        if descriptor.is_null() {
            return Err(io::Error::other("security query returned no descriptor"));
        }
        Ok(OwnedDescriptor(descriptor))
    }

    unsafe fn validate_descriptor(
        descriptor: PSECURITY_DESCRIPTOR,
        current_user: PSID,
        system: PSID,
        administrators: PSID,
        require_child_inheritance: bool,
    ) -> io::Result<bool> {
        let mut control = 0_u16;
        let mut revision = 0_u32;
        // SAFETY: descriptor and output pointers are valid for this call.
        if unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Ok(false);
        }
        let mut owner = ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: descriptor and outputs are valid.
        if unsafe { GetSecurityDescriptorOwner(descriptor, &raw mut owner, &raw mut defaulted) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        if owner.is_null() || unsafe { EqualSid(owner, current_user) } == 0 {
            return Ok(false);
        }
        let mut present = 0;
        let mut dacl = ptr::null_mut();
        // SAFETY: descriptor and outputs are valid.
        if unsafe {
            GetSecurityDescriptorDacl(
                descriptor,
                &raw mut present,
                &raw mut dacl,
                &raw mut defaulted,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if present == 0 || dacl.is_null() {
            return Ok(false);
        }
        // SAFETY: successful descriptor query returned a live ACL header.
        let acl_revision = unsafe { (*dacl).AclRevision };
        if !matches!(u32::from(acl_revision), ACL_REVISION | ACL_REVISION_DS) {
            return Ok(false);
        }
        // SAFETY: successful descriptor query returned a live ACL.
        let count = unsafe { (*dacl).AceCount };
        for index in 0..u32::from(count) {
            let mut raw_ace = ptr::null_mut();
            // SAFETY: index is bounded by AceCount.
            if unsafe { GetAce(dacl, index, &raw mut raw_ace) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: GetAce returned a valid ACE_HEADER.
            let header = unsafe { &*raw_ace.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
            if u32::from(header.AceFlags) & INHERITED_ACE != 0 {
                return Ok(false);
            }
            match u32::from(header.AceType) {
                ACCESS_ALLOWED_ACE_TYPE => {
                    if require_child_inheritance
                        && u32::from(header.AceFlags) & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE)
                            != OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
                    {
                        return Ok(false);
                    }
                    let Some(sid) = (unsafe {
                        checked_ace_sid(raw_ace, header, offset_of!(ACCESS_ALLOWED_ACE, SidStart))
                    }) else {
                        return Ok(false);
                    };
                    if unsafe { EqualSid(sid, current_user) } == 0
                        && unsafe { EqualSid(sid, system) } == 0
                        && unsafe { EqualSid(sid, administrators) } == 0
                    {
                        return Ok(false);
                    }
                }
                ACCESS_DENIED_ACE_TYPE => {
                    if unsafe {
                        checked_ace_sid(raw_ace, header, offset_of!(ACCESS_DENIED_ACE, SidStart))
                    }
                    .is_none()
                    {
                        return Ok(false);
                    }
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    unsafe fn checked_ace_sid(
        raw_ace: *mut c_void,
        header: &windows_sys::Win32::Security::ACE_HEADER,
        sid_offset: usize,
    ) -> Option<PSID> {
        const MIN_SID_BYTES: usize = 8;
        let ace_size = usize::from(header.AceSize);
        if ace_size < sid_offset.checked_add(MIN_SID_BYTES)? {
            return None;
        }
        // SAFETY: the bounds check above proves the fixed SID header is inside the ACE.
        let sid = unsafe { raw_ace.cast::<u8>().add(sid_offset).cast::<c_void>() };
        // A SID is an 8-byte fixed header followed by four bytes per
        // sub-authority. Bound that count before asking Win32 to inspect it.
        // SAFETY: the fixed SID header was proven inside the ACE above.
        let sub_authority_count = usize::from(unsafe { *sid.cast::<u8>().add(1) });
        let structural_length = MIN_SID_BYTES.checked_add(sub_authority_count.checked_mul(4)?)?;
        if sid_offset.checked_add(structural_length)? > ace_size {
            return None;
        }
        // SAFETY: the fixed SID header is in bounds, so IsValidSid may inspect it.
        if unsafe { IsValidSid(sid) } == 0 {
            return None;
        }
        // SAFETY: IsValidSid accepted the SID structure.
        let sid_length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid) } as usize;
        sid_offset
            .checked_add(sid_length)
            .filter(|end| *end <= ace_size)
            .map(|_| sid)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::Security::{
            AddAce, InitializeSecurityDescriptor, SetSecurityDescriptorControl,
            SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, WinWorldSid, ACE_HEADER,
            SECURITY_DESCRIPTOR,
        };
        use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

        #[test]
        fn hardening_produces_a_private_held_file() {
            let file = tempfile::tempfile().expect("temporary file");
            harden_private_handle(&file, ObjectKind::File).expect("harden");
            assert!(is_private_handle(&file, ObjectKind::File, 1).expect("validate"));
        }

        #[test]
        fn inherited_default_acl_is_not_accepted_as_private() {
            let file = tempfile::tempfile().expect("temporary file");
            assert!(!is_private_handle(&file, ObjectKind::File, 1).expect("validate"));
        }

        #[test]
        fn descriptor_parser_rejects_absent_and_null_dacl() {
            let user = current_user_sid().expect("user SID");
            assert!(!validate_synthetic(user.as_sid(), None, false, true));
            assert!(!validate_synthetic(user.as_sid(), None, true, true));
        }

        #[test]
        fn descriptor_parser_rejects_unprotected_inherited_broad_and_unknown_aces() {
            let user = current_user_sid().expect("user SID");
            let world = well_known_sid(WinWorldSid).expect("world SID");
            let mut ordinary = acl_for_sid(user.as_sid(), 0);
            assert!(!validate_synthetic(
                user.as_sid(),
                Some(ordinary.as_mut_sid().cast()),
                true,
                false
            ));
            let mut inherited = acl_for_sid(user.as_sid(), INHERITED_ACE);
            assert!(!validate_synthetic(
                user.as_sid(),
                Some(inherited.as_mut_sid().cast()),
                true,
                true
            ));
            let mut broad = acl_for_sid(world.as_sid(), 0);
            assert!(!validate_synthetic(
                user.as_sid(),
                Some(broad.as_mut_sid().cast()),
                true,
                true
            ));
            let mut unknown = unknown_acl();
            assert!(!validate_synthetic(
                user.as_sid(),
                Some(unknown.as_mut_sid().cast()),
                true,
                true
            ));
        }

        #[test]
        fn descriptor_parser_rejects_owner_mismatch() {
            let user = current_user_sid().expect("user SID");
            let system = well_known_sid(WinLocalSystemSid).expect("system SID");
            let mut acl = acl_for_sid(user.as_sid(), 0);
            assert!(!validate_synthetic(
                system.as_sid(),
                Some(acl.as_mut_sid().cast()),
                true,
                true
            ));
        }

        fn acl_for_sid(sid: PSID, flags: u32) -> SidBuffer {
            let sid_length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid) } as usize;
            let bytes = size_of::<ACL>() + offset_of!(ACCESS_ALLOWED_ACE, SidStart) + sid_length;
            let mut storage = SidBuffer::with_bytes(bytes).expect("ACL storage");
            let acl = storage.as_mut_sid().cast::<ACL>();
            assert_ne!(
                unsafe { InitializeAcl(acl, u32::try_from(bytes).unwrap(), ACL_REVISION) },
                0
            );
            assert_ne!(
                unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, flags, FILE_ALL_ACCESS, sid) },
                0
            );
            storage
        }

        fn unknown_acl() -> SidBuffer {
            let bytes = size_of::<ACL>() + size_of::<ACE_HEADER>();
            let mut storage = SidBuffer::with_bytes(bytes).expect("ACL storage");
            let acl = storage.as_mut_sid().cast::<ACL>();
            assert_ne!(
                unsafe { InitializeAcl(acl, u32::try_from(bytes).unwrap(), ACL_REVISION) },
                0
            );
            let header = ACE_HEADER {
                AceType: 0x7f,
                AceFlags: 0,
                AceSize: u16::try_from(size_of::<ACE_HEADER>()).unwrap(),
            };
            assert_ne!(
                unsafe {
                    AddAce(
                        acl,
                        ACL_REVISION,
                        u32::MAX,
                        ptr::from_ref(&header).cast(),
                        u32::try_from(size_of::<ACE_HEADER>()).unwrap(),
                    )
                },
                0
            );
            storage
        }

        fn validate_synthetic(
            owner: PSID,
            dacl: Option<*mut ACL>,
            present: bool,
            protected: bool,
        ) -> bool {
            let mut descriptor = SECURITY_DESCRIPTOR::default();
            let descriptor_ptr = (&raw mut descriptor).cast::<c_void>();
            assert_ne!(
                unsafe {
                    InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION)
                },
                0
            );
            assert_ne!(
                unsafe { SetSecurityDescriptorOwner(descriptor_ptr, owner, 0) },
                0
            );
            assert_ne!(
                unsafe {
                    SetSecurityDescriptorDacl(
                        descriptor_ptr,
                        i32::from(present),
                        dacl.unwrap_or(ptr::null_mut()),
                        0,
                    )
                },
                0
            );
            if protected {
                assert_ne!(
                    unsafe {
                        SetSecurityDescriptorControl(
                            descriptor_ptr,
                            SE_DACL_PROTECTED,
                            SE_DACL_PROTECTED,
                        )
                    },
                    0
                );
            }
            let user = current_user_sid().expect("user SID");
            let system = well_known_sid(WinLocalSystemSid).expect("system SID");
            let administrators =
                well_known_sid(WinBuiltinAdministratorsSid).expect("administrators SID");
            unsafe {
                validate_descriptor(
                    descriptor_ptr,
                    user.as_sid(),
                    system.as_sid(),
                    administrators.as_sid(),
                    false,
                )
                .expect("parse descriptor")
            }
        }
    }
}

#[cfg(windows)]
pub use windows::{harden_private_handle, is_non_reparse_handle, is_private_handle, ObjectKind};

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn ordinary_temporary_file_has_no_extended_acl() {
        let file = tempfile::tempfile().expect("temporary file");
        assert!(!has_extended_acl(&file).expect("inspect ACL"));
    }
}
