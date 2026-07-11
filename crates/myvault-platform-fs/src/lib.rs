//! Isolated operating-system filesystem primitives for myVault.

use std::ffi::OsStr;
use std::io;

use cap_std::fs::Dir;

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

#[cfg(all(test, windows))]
mod windows_tests {
    use std::ffi::OsStr;
    use std::fs;

    use cap_std::ambient_authority;
    use cap_std::fs::Dir;

    use super::rename_noreplace;

    fn fixture() -> (tempfile::TempDir, Dir) {
        let root = tempfile::tempdir().expect("temporary directory");
        let directory = Dir::open_ambient_dir(root.path(), ambient_authority()).expect("open root");
        (root, directory)
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
}
