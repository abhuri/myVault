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
