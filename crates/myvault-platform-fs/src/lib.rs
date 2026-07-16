//! Isolated operating-system filesystem primitives for myVault.

use std::ffi::OsStr;
use std::fmt;
use std::io;

use cap_std::fs::Dir;

/// Opaque, platform-complete identity of an opened directory handle.
///
/// Unix stores the device and inode. Windows stores the volume serial number
/// and the complete 128-bit file identifier returned by `FileIdInfo`.
/// This is exact evidence about a *currently held handle*, not restart-stable
/// durable evidence. Use [`DirectoryIdentity::held_object_identity_token`] to
/// carry this distinction across a platform boundary.
#[derive(Clone, Eq, PartialEq)]
pub struct DirectoryIdentity {
    volume: u64,
    file_id: [u8; 16],
}

/// Platform-complete identity of an opened regular file handle.
///
/// This is exact evidence about a *currently held handle*, not restart-stable
/// durable evidence.
#[derive(Clone, Eq, PartialEq)]
pub struct FileIdentity {
    volume: u64,
    file_id: [u8; 16],
}

/// The version of [`HeldObjectIdentityToken`] emitted by this crate.
pub const HELD_OBJECT_IDENTITY_TOKEN_VERSION: u8 = 1;

const HELD_OBJECT_IDENTITY_TOKEN_BYTES: usize = 26;
const DIRECTORY_IDENTITY_KIND: u8 = 1;
const FILE_IDENTITY_KIND: u8 = 2;

/// The held-handle object type encoded in a [`HeldObjectIdentityToken`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeldObjectKind {
    Directory,
    File,
}

impl HeldObjectKind {
    const fn encoded(self) -> u8 {
        match self {
            Self::Directory => DIRECTORY_IDENTITY_KIND,
            Self::File => FILE_IDENTITY_KIND,
        }
    }

    const fn decode(value: u8) -> Result<Self, IdentityTokenError> {
        match value {
            DIRECTORY_IDENTITY_KIND => Ok(Self::Directory),
            FILE_IDENTITY_KIND => Ok(Self::File),
            _ => Err(IdentityTokenError::UnsupportedKind),
        }
    }
}

/// A canonical, versioned identity token for one currently held object handle.
///
/// The token contains every byte of the platform's current object identity in
/// a deterministic wire form: version, kind, big-endian volume, and the full
/// 128-bit file identifier. It deliberately makes no restart-stability or
/// durability claim; a durable contract must obtain independent evidence from
/// a verifier/provider after this token is captured.
#[derive(Clone, Eq, PartialEq)]
pub struct HeldObjectIdentityToken([u8; HELD_OBJECT_IDENTITY_TOKEN_BYTES]);

impl HeldObjectIdentityToken {
    /// Parses exactly one canonical token and rejects truncated, unsupported,
    /// or forward-version input rather than weakening the identity proof.
    ///
    /// # Errors
    /// Returns a redacted error when the token is not exactly the supported
    /// canonical length, version, and kind.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, IdentityTokenError> {
        let bytes: [u8; HELD_OBJECT_IDENTITY_TOKEN_BYTES] = bytes
            .try_into()
            .map_err(|_| IdentityTokenError::InvalidLength)?;
        if bytes[0] != HELD_OBJECT_IDENTITY_TOKEN_VERSION {
            return Err(IdentityTokenError::UnsupportedVersion);
        }
        HeldObjectKind::decode(bytes[1])?;
        Ok(Self(bytes))
    }

    /// Returns the exact canonical token bytes for a trusted boundary.
    ///
    /// Callers must treat the bytes as sensitive identity evidence. Normal
    /// formatting is redacted and never exposes these bytes.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub const fn version(&self) -> u8 {
        self.0[0]
    }

    #[must_use]
    ///
    /// # Panics
    /// Panics only if this crate's private validated-token invariant is broken.
    pub fn kind(&self) -> HeldObjectKind {
        // Construction and parsing validate this byte.
        HeldObjectKind::decode(self.0[1]).expect("held identity token kind was validated")
    }

    fn new(kind: HeldObjectKind, volume: u64, file_id: [u8; 16]) -> Self {
        let mut bytes = [0_u8; HELD_OBJECT_IDENTITY_TOKEN_BYTES];
        bytes[0] = HELD_OBJECT_IDENTITY_TOKEN_VERSION;
        bytes[1] = kind.encoded();
        bytes[2..10].copy_from_slice(&volume.to_be_bytes());
        bytes[10..].copy_from_slice(&file_id);
        Self(bytes)
    }
}

impl fmt::Debug for HeldObjectIdentityToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HeldObjectIdentityToken")
            .field("version", &self.version())
            .field("kind", &self.kind())
            .field("identity_bytes", &"<redacted>")
            .finish()
    }
}

impl fmt::Debug for DirectoryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DirectoryIdentity(<redacted held-handle identity>)")
    }
}

impl fmt::Debug for FileIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FileIdentity(<redacted held-handle identity>)")
    }
}

/// Redacted parsing failure for a held identity token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityTokenError {
    InvalidLength,
    UnsupportedVersion,
    UnsupportedKind,
}

impl fmt::Display for IdentityTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLength => "held identity token has an invalid length",
            Self::UnsupportedVersion => "held identity token version is unsupported",
            Self::UnsupportedKind => "held identity token kind is unsupported",
        })
    }
}

impl std::error::Error for IdentityTokenError {}

/// Opaque identity of the mount instance containing a held directory.
///
/// Linux distinguishes kernel-unique mount ids from the legacy id returned by
/// older kernels. macOS retains the complete filesystem id and both mount
/// endpoint names from `fstatfs`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountIdentity(MountIdentityInner);

#[derive(Clone, Debug, Eq, PartialEq)]
enum MountIdentityInner {
    #[cfg(target_os = "linux")]
    Linux { id: u64, unique: bool },
    #[cfg(target_os = "macos")]
    Macos {
        fsid: [i32; 2],
        mounted_on: Box<[i8]>,
        mounted_from: Box<[i8]>,
    },
}

impl DirectoryIdentity {
    const fn new(volume: u64, file_id: [u8; 16]) -> Self {
        Self { volume, file_id }
    }

    /// Exports a canonical token for exact comparison of this held directory.
    ///
    /// This token is not independently restart-stable evidence.
    #[must_use]
    pub fn held_object_identity_token(&self) -> HeldObjectIdentityToken {
        HeldObjectIdentityToken::new(HeldObjectKind::Directory, self.volume, self.file_id)
    }
}

impl FileIdentity {
    /// Exports a canonical token for exact comparison of this held file.
    ///
    /// This token is not independently restart-stable evidence.
    #[must_use]
    pub fn held_object_identity_token(&self) -> HeldObjectIdentityToken {
        HeldObjectIdentityToken::new(HeldObjectKind::File, self.volume, self.file_id)
    }
}

/// Reads the identity of an already-open directory without consulting an
/// ambient path.
///
/// # Errors
///
/// Returns the operating-system error when a complete handle identity cannot
/// be obtained. Callers must fail closed rather than fall back to a truncated
/// identifier.
pub fn directory_identity(directory: &Dir) -> io::Result<DirectoryIdentity> {
    platform::directory_identity(directory)
}

/// Reads a complete identity from an already-open file handle.
///
/// # Errors
/// Returns the platform error when complete identity cannot be obtained.
pub fn file_identity(file: &cap_std::fs::File) -> io::Result<FileIdentity> {
    #[cfg(windows)]
    {
        platform::file_identity(file)
    }
    #[cfg(not(windows))]
    {
        use cap_fs_ext::MetadataExt;
        let metadata = file.metadata()?;
        let mut file_id = [0_u8; 16];
        file_id[..8].copy_from_slice(&metadata.ino().to_be_bytes());
        Ok(FileIdentity {
            volume: metadata.dev(),
            file_id,
        })
    }
}

/// Reads a mount-instance identity exclusively from a held directory.
///
/// Linux prefers `STATX_MNT_ID_UNIQUE` and falls back to `STATX_MNT_ID` only
/// when the kernel does not report the unique field. macOS combines `fstat`
/// type validation with the complete held `fstatfs` mount identity.
///
/// # Errors
/// Returns [`io::ErrorKind::Unsupported`] rather than weakening the proof when
/// the target or kernel cannot provide a mount-instance identity.
pub fn mount_identity(directory: &Dir) -> io::Result<MountIdentity> {
    platform_mount_identity(directory)
}

#[cfg(target_os = "linux")]
fn platform_mount_identity(directory: &Dir) -> io::Result<MountIdentity> {
    use rustix::fs::{AtFlags, StatxFlags};

    // Linux 6.8 added STATX_MNT_ID_UNIQUE. rustix 1.1 exposes forward-compatible
    // bitflags but does not yet name this bit, so retain the UAPI value here.
    const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
    let unique = StatxFlags::from_bits_retain(STATX_MNT_ID_UNIQUE);
    match rustix::fs::statx(directory, "", AtFlags::EMPTY_PATH, unique) {
        Ok(stat) if stat.stx_mask & STATX_MNT_ID_UNIQUE != 0 => {
            return Ok(MountIdentity(MountIdentityInner::Linux {
                id: stat.stx_mnt_id,
                unique: true,
            }));
        }
        Ok(_) | Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS) => {}
        Err(error) => return Err(io::Error::from(error)),
    }

    let legacy = rustix::fs::statx(directory, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        .map_err(io::Error::from)?;
    if legacy.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "statx did not report a mount id",
        ));
    }
    Ok(MountIdentity(MountIdentityInner::Linux {
        id: legacy.stx_mnt_id,
        unique: false,
    }))
}

#[cfg(target_os = "macos")]
fn platform_mount_identity(directory: &Dir) -> io::Result<MountIdentity> {
    let held = directory.try_clone()?.into_std_file();
    let stat = rustix::fs::fstat(&held)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "mount identity requires a held directory",
        ));
    }
    let statfs = rustix::fs::fstatfs(&held)?;
    // SAFETY: Darwin fsid_t is exactly two i32 values. Both source and target
    // are plain Copy values of identical size/alignment, and every bit pattern
    // is valid for i32. This platform crate isolates the libc-private field.
    let fsid = unsafe { std::mem::transmute::<rustix::fs::Fsid, [i32; 2]>(statfs.f_fsid) };
    Ok(MountIdentity(MountIdentityInner::Macos {
        fsid,
        mounted_on: nul_terminated_mount_name(&statfs.f_mntonname),
        mounted_from: nul_terminated_mount_name(&statfs.f_mntfromname),
    }))
}

#[cfg(target_os = "macos")]
fn nul_terminated_mount_name(name: &[i8]) -> Box<[i8]> {
    let length = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    name[..length].into()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_mount_identity(_directory: &Dir) -> io::Result<MountIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "held mount identity is unavailable on this target",
    ))
}

/// Atomically renames an entry without replacing an existing destination.
///
/// # Errors
///
/// Returns [`io::ErrorKind::AlreadyExists`] when the destination exists and
/// [`io::ErrorKind::Unsupported`] on platforms without a safe implementation.
pub fn rename_noreplace(
    source_parent: &Dir,
    source_name: &OsStr,
    destination_parent: &Dir,
    destination_name: &OsStr,
) -> io::Result<()> {
    platform::rename_noreplace(
        source_parent,
        source_name,
        destination_parent,
        destination_name,
    )
}

#[cfg(windows)]
mod platform;

#[cfg(not(windows))]
mod platform {
    use std::ffi::OsStr;
    use std::io;

    use cap_std::fs::Dir;

    use cap_fs_ext::MetadataExt;

    use super::DirectoryIdentity;

    pub(super) fn directory_identity(directory: &Dir) -> io::Result<DirectoryIdentity> {
        let metadata = directory.dir_metadata()?;
        let mut file_id = [0_u8; 16];
        file_id[..8].copy_from_slice(&metadata.ino().to_be_bytes());
        Ok(DirectoryIdentity::new(metadata.dev(), file_id))
    }

    pub(super) fn rename_noreplace(
        _source_parent: &Dir,
        _source_name: &OsStr,
        _destination_parent: &Dir,
        _destination_name: &OsStr,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows handle-relative rename is unavailable on this platform",
        ))
    }
}

#[cfg(test)]
mod identity_tests {
    use super::{
        DirectoryIdentity, FileIdentity, HeldObjectIdentityToken, HeldObjectKind,
        IdentityTokenError, HELD_OBJECT_IDENTITY_TOKEN_VERSION,
    };

    #[test]
    fn full_identifier_distinguishes_equal_low_64_bits() {
        let low = 0x0123_4567_89ab_cdef_u64.to_ne_bytes();
        let mut first = [0_u8; 16];
        first[..8].copy_from_slice(&low);
        first[8..].copy_from_slice(&1_u64.to_ne_bytes());
        let mut second = [0_u8; 16];
        second[..8].copy_from_slice(&low);
        second[8..].copy_from_slice(&2_u64.to_ne_bytes());

        assert_ne!(
            DirectoryIdentity::new(7, first),
            DirectoryIdentity::new(7, second)
        );
        assert_ne!(
            DirectoryIdentity::new(7, first).held_object_identity_token(),
            DirectoryIdentity::new(7, second).held_object_identity_token()
        );
    }

    #[test]
    fn held_token_is_versioned_kind_separated_and_redacted() {
        let file_id = [0x5a; 16];
        let first = DirectoryIdentity::new(7, file_id).held_object_identity_token();
        let second = DirectoryIdentity::new(7, file_id).held_object_identity_token();
        let file = FileIdentity { volume: 7, file_id }.held_object_identity_token();

        assert_eq!(first, second);
        assert_ne!(first, file);
        assert_eq!(first.version(), HELD_OBJECT_IDENTITY_TOKEN_VERSION);
        assert_eq!(first.kind(), HeldObjectKind::Directory);
        assert_eq!(file.kind(), HeldObjectKind::File);
        assert_eq!(first.canonical_bytes().len(), 26);
        assert_eq!(&first.canonical_bytes()[2..10], &7_u64.to_be_bytes());
        assert_eq!(&first.canonical_bytes()[10..], &file_id);
        assert_ne!(
            format!("{first:?}"),
            format!("{:?}", first.canonical_bytes())
        );
        assert!(!format!("{first:?}").contains("5a"));
    }

    #[test]
    fn held_token_rejects_truncation_and_unsupported_headers() {
        let token = DirectoryIdentity::new(7, [1; 16]).held_object_identity_token();
        assert_eq!(
            HeldObjectIdentityToken::from_canonical_bytes(&token.canonical_bytes()[..25]),
            Err(IdentityTokenError::InvalidLength)
        );

        let mut unsupported_version = token.canonical_bytes().to_vec();
        unsupported_version[0] += 1;
        assert_eq!(
            HeldObjectIdentityToken::from_canonical_bytes(&unsupported_version),
            Err(IdentityTokenError::UnsupportedVersion)
        );

        let mut unsupported_kind = token.canonical_bytes().to_vec();
        unsupported_kind[1] = 99;
        assert_eq!(
            HeldObjectIdentityToken::from_canonical_bytes(&unsupported_kind),
            Err(IdentityTokenError::UnsupportedKind)
        );
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod mount_identity_tests {
    use cap_std::ambient_authority;
    use cap_std::fs::Dir;

    use super::mount_identity;

    #[test]
    fn held_mount_identity_is_stable_across_independent_opens() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let canonical = temporary.path().canonicalize().expect("canonical path");
        let first = Dir::open_ambient_dir(&canonical, ambient_authority()).expect("first open");
        let second = Dir::open_ambient_dir(&canonical, ambient_authority()).expect("second open");

        assert_eq!(
            mount_identity(&first).expect("first mount identity"),
            mount_identity(&second).expect("second mount identity")
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::ffi::OsStr;
    use std::fs;

    use cap_std::ambient_authority;
    use cap_std::fs::Dir;

    use super::{directory_identity, rename_noreplace};

    fn fixture() -> (tempfile::TempDir, Dir) {
        let root = tempfile::tempdir().expect("temporary directory");
        let directory = Dir::open_ambient_dir(root.path(), ambient_authority()).expect("open root");
        (root, directory)
    }

    #[test]
    fn directory_identity_is_stable_and_distinguishes_open_directories() {
        let (root, directory) = fixture();
        fs::create_dir(root.path().join("first")).expect("first directory");
        fs::create_dir(root.path().join("second")).expect("second directory");
        let first = directory.open_dir("first").expect("open first");
        let first_again = directory.open_dir("first").expect("reopen first");
        let second = directory.open_dir("second").expect("open second");

        assert_eq!(
            directory_identity(&first).expect("first identity"),
            directory_identity(&first_again).expect("reopened first identity")
        );
        assert_ne!(
            directory_identity(&first).expect("first identity"),
            directory_identity(&second).expect("second identity")
        );
    }

    #[test]
    fn directory_identity_follows_rename_and_rejects_path_replacement() {
        let (root, directory) = fixture();
        fs::create_dir(root.path().join("item")).expect("item directory");
        let held = directory.open_dir("item").expect("open item");
        let held_identity = directory_identity(&held).expect("held identity");

        fs::rename(root.path().join("item"), root.path().join("detached")).expect("detach item");
        fs::create_dir(root.path().join("item")).expect("replacement item");
        let detached = directory.open_dir("detached").expect("open detached");
        let replacement = directory.open_dir("item").expect("open replacement");

        assert_eq!(
            held_identity,
            directory_identity(&detached).expect("detached identity")
        );
        assert_ne!(
            held_identity,
            directory_identity(&replacement).expect("replacement identity")
        );
    }

    #[test]
    fn moves_one_character_name_in_same_parent() {
        let (root, directory) = fixture();
        fs::write(root.path().join("a"), b"source").expect("source");

        rename_noreplace(&directory, OsStr::new("a"), &directory, OsStr::new("ข"))
            .expect("one-character rename");

        assert!(!root.path().join("a").exists());
        assert_eq!(
            fs::read(root.path().join("ข")).expect("destination"),
            b"source"
        );
    }

    #[test]
    fn moves_unicode_name_between_held_parents() {
        let (root, directory) = fixture();
        fs::create_dir(root.path().join("ต้นทาง")).expect("source parent");
        fs::create_dir(root.path().join("ปลายทาง")).expect("destination parent");
        fs::write(root.path().join("ต้นทาง/บันทึก😀.md"), "สวัสดี").expect("source");
        let source = directory.open_dir("ต้นทาง").expect("open source parent");
        let destination = directory
            .open_dir("ปลายทาง")
            .expect("open destination parent");

        rename_noreplace(
            &source,
            OsStr::new("บันทึก😀.md"),
            &destination,
            OsStr::new("ย้ายแล้ว🗒️.md"),
        )
        .expect("cross-parent Unicode rename");

        assert!(!root.path().join("ต้นทาง/บันทึก😀.md").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("ปลายทาง/ย้ายแล้ว🗒️.md")).expect("destination"),
            "สวัสดี"
        );
    }

    #[test]
    fn existing_file_destination_is_preserved() {
        let (root, directory) = fixture();
        fs::write(root.path().join("source.txt"), b"source").expect("source");
        fs::write(root.path().join("destination.txt"), b"keep").expect("destination");

        let error = rename_noreplace(
            &directory,
            OsStr::new("source.txt"),
            &directory,
            OsStr::new("destination.txt"),
        )
        .expect_err("destination must not be replaced");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(root.path().join("source.txt")).expect("source"),
            b"source"
        );
        assert_eq!(
            fs::read(root.path().join("destination.txt")).expect("destination"),
            b"keep"
        );
    }

    #[test]
    fn existing_directory_destination_is_preserved() {
        let (root, directory) = fixture();
        fs::create_dir(root.path().join("source-dir")).expect("source directory");
        fs::create_dir(root.path().join("destination-dir")).expect("destination directory");
        fs::write(root.path().join("source-dir/source.txt"), b"source").expect("source file");
        fs::write(root.path().join("destination-dir/keep.txt"), b"keep").expect("destination file");

        let error = rename_noreplace(
            &directory,
            OsStr::new("source-dir"),
            &directory,
            OsStr::new("destination-dir"),
        )
        .expect_err("destination directory must not be replaced");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(root.path().join("source-dir/source.txt")).expect("source file"),
            b"source"
        );
        assert_eq!(
            fs::read(root.path().join("destination-dir/keep.txt")).expect("destination file"),
            b"keep"
        );
    }

    #[test]
    fn reparse_source_is_rejected_before_native_rename() {
        use std::os::windows::fs::symlink_file;

        let (root, directory) = fixture();
        fs::write(root.path().join("target.txt"), b"target").expect("target");
        if let Err(error) = symlink_file(
            root.path().join("target.txt"),
            root.path().join("source-link.txt"),
        ) {
            // Some local Windows configurations do not grant symlink creation.
            // GitHub-hosted Windows runners exercise the assertion below.
            assert!(matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ));
            return;
        }

        let error = rename_noreplace(
            &directory,
            OsStr::new("source-link.txt"),
            &directory,
            OsStr::new("destination.txt"),
        )
        .expect_err("reparse source must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(root.path().join("source-link.txt").exists());
        assert!(!root.path().join("destination.txt").exists());
        assert_eq!(
            fs::read(root.path().join("target.txt")).expect("target"),
            b"target"
        );
    }
}
