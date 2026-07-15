use std::{fs, io};

use myvault_core::{
    ContentPublishOutcome, CoreError, FileRevision, Sha256Digest, Vault, VaultPath,
};

const MAX_TRANSFER: usize = 8 * 1024 * 1024;

#[test]
fn unicode_zero_byte_and_large_binary_stream_byte_exactly() {
    let root = tempfile::tempdir().unwrap();
    let vault = open_vault(&root);
    for (path, bytes) in [
        ("ว่างเปล่า.md", Vec::new()),
        ("ไฟล์/ภาพ 🧪.bin", binary_fixture(5 * 1024 * 1024 + 17)),
    ] {
        let path = VaultPath::from_portable(path).unwrap();
        let digest = Sha256Digest::from_bytes(&bytes);
        let outcome = vault
            .create_content_from_reader(
                &path,
                &mut bytes.as_slice(),
                &digest,
                bytes.len() as u64,
                MAX_TRANSFER,
            )
            .unwrap();
        assert!(matches!(outcome, ContentPublishOutcome::Created(_)));
        let mut readback = Vec::new();
        let snapshot = vault
            .stream_content_snapshot(&path, &mut readback, MAX_TRANSFER)
            .unwrap();
        assert_eq!(snapshot.sha256, digest);
        assert_eq!(snapshot.byte_len, bytes.len() as u64);
        assert_eq!(readback, bytes);
    }
}

#[test]
fn create_never_replaces_and_rejects_wrong_digest_before_publication() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("existing.bin"), b"preserve").unwrap();
    let vault = open_vault(&root);
    let existing = VaultPath::from_portable("existing.bin").unwrap();
    assert!(matches!(
        vault.create_content_from_reader(
            &existing,
            &mut b"replace".as_slice(),
            &Sha256Digest::from_bytes(b"replace"),
            7,
            128,
        ),
        Err(CoreError::AlreadyExists(_))
    ));
    let fresh = VaultPath::from_portable("fresh.bin").unwrap();
    assert!(matches!(
        vault.create_content_from_reader(
            &fresh,
            &mut b"payload".as_slice(),
            &Sha256Digest::from_bytes(b"different"),
            7,
            128,
        ),
        Err(CoreError::ContentDigestMismatch)
    ));
    assert_eq!(
        fs::read(root.path().join("existing.bin")).unwrap(),
        b"preserve"
    );
    assert!(!root.path().join("fresh.bin").exists());
}

#[test]
fn replace_requires_exact_current_revision_and_preserves_stale_target() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("note.md"), b"current").unwrap();
    let vault = open_vault(&root);
    let path = VaultPath::from_portable("note.md").unwrap();
    let replacement = b"replacement\0binary";
    assert!(matches!(
        vault.replace_content_from_reader_if_revision(
            &path,
            &FileRevision::from_bytes(b"stale"),
            &mut replacement.as_slice(),
            &Sha256Digest::from_bytes(replacement),
            replacement.len() as u64,
            128,
        ),
        Err(CoreError::StaleRevision { .. })
    ));
    assert_eq!(fs::read(root.path().join("note.md")).unwrap(), b"current");

    let error = vault
        .replace_content_from_reader_if_revision(
            &path,
            &FileRevision::from_bytes(b"current"),
            &mut replacement.as_slice(),
            &Sha256Digest::from_bytes(replacement),
            replacement.len() as u64,
            128,
        )
        .expect_err("existing-target replacement is fail-closed in R2");
    assert!(matches!(
        error,
        CoreError::ExistingContentReplaceUnsupported
    ));
    assert_eq!(fs::read(root.path().join("note.md")).unwrap(), b"current");
}

#[test]
fn protected_paths_and_interrupted_streams_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".obsidian")).unwrap();
    fs::write(root.path().join(".obsidian/config"), b"private").unwrap();
    let vault = open_vault(&root);
    let protected = VaultPath::from_portable(".obsidian/config").unwrap();
    assert!(vault
        .stream_content_snapshot(&protected, &mut Vec::new(), 128)
        .is_err());

    let path = VaultPath::from_portable("partial.bin").unwrap();
    let mut reader = FailingReader { emitted: false };
    assert!(matches!(
        vault.create_content_from_reader(
            &path,
            &mut reader,
            &Sha256Digest::from_bytes(b"prefix"),
            6,
            128,
        ),
        Err(CoreError::Io(_))
    ));
    assert!(!root.path().join("partial.bin").exists());
}

struct FailingReader {
    emitted: bool,
}

impl io::Read for FailingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.emitted {
            return Err(io::Error::other("injected transfer interruption"));
        }
        self.emitted = true;
        buffer[..6].copy_from_slice(b"prefix");
        Ok(6)
    }
}

fn binary_fixture(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| u8::try_from((index * 131 + 17) % 256).unwrap())
        .collect()
}

fn open_vault(root: &tempfile::TempDir) -> Vault {
    Vault::open(fs::canonicalize(root.path()).unwrap()).unwrap()
}
