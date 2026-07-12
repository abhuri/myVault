use std::fs;

use myvault_core::{CoreError, FileRevision, Vault, VaultPath, MAX_NOTE_BYTES};

#[test]
fn reads_exact_bytes_and_revision_from_one_note_stream() {
    let temporary = tempfile::tempdir().expect("temporary");
    let root = temporary.path().canonicalize().expect("canonical root");
    fs::create_dir(root.join("บันทึก")).expect("note directory");
    let bytes = "# สวัสดี\nเนื้อหาภาษาไทย\n".as_bytes();
    fs::write(root.join("บันทึก/วันนี้.md"), bytes).expect("note");
    let vault = Vault::open(&root).expect("vault");
    let path = VaultPath::from_portable("บันทึก/วันนี้.md").expect("portable path");

    let read = vault.read_note_with_revision(&path).expect("read note");

    assert_eq!(read.bytes, bytes);
    assert_eq!(read.revision, FileRevision::from_bytes(bytes));
}

#[test]
fn accepts_empty_and_exact_limit_notes_but_rejects_larger_content() {
    let temporary = tempfile::tempdir().expect("temporary");
    let root = temporary.path().canonicalize().expect("canonical root");
    fs::write(root.join("empty.md"), []).expect("empty note");
    fs::write(root.join("limit.MD"), vec![b'a'; MAX_NOTE_BYTES]).expect("limit note");
    fs::write(root.join("large.md"), vec![b'b'; MAX_NOTE_BYTES + 1]).expect("large note");
    let vault = Vault::open(&root).expect("vault");

    assert!(vault
        .read_note_with_revision(&VaultPath::from_portable("empty.md").expect("path"))
        .expect("empty")
        .bytes
        .is_empty());
    assert_eq!(
        vault
            .read_note_with_revision(&VaultPath::from_portable("limit.MD").expect("path"))
            .expect("limit")
            .bytes
            .len(),
        MAX_NOTE_BYTES
    );
    assert!(matches!(
        vault.read_note_with_revision(&VaultPath::from_portable("large.md").expect("path")),
        Err(CoreError::ResourceLimitExceeded { .. })
    ));
}

#[test]
fn rejects_non_markdown_internal_directory_and_hardlinked_targets() {
    let temporary = tempfile::tempdir().expect("temporary");
    let root = temporary.path().canonicalize().expect("canonical root");
    fs::write(root.join("note.txt"), b"text").expect("text");
    fs::write(root.join("mixed.Md"), b"text").expect("mixed extension");
    fs::create_dir(root.join("folder.md")).expect("directory target");
    fs::create_dir(root.join(".obsidian")).expect("obsidian");
    fs::write(root.join(".obsidian/private.md"), b"private").expect("private note");
    fs::create_dir_all(root.join(".trash/v1")).expect("trash");
    fs::write(root.join(".trash/v1/private.md"), b"private").expect("trash note");
    fs::write(root.join("linked.md"), b"linked").expect("linked note");
    fs::hard_link(root.join("linked.md"), root.join("alias.md")).expect("hard link");
    let vault = Vault::open(&root).expect("vault");

    for path in [
        "note.txt",
        "mixed.Md",
        "folder.md",
        ".obsidian/private.md",
        ".trash/v1/private.md",
        "linked.md",
    ] {
        assert!(vault
            .read_note_with_revision(&VaultPath::from_portable(path).expect("portable path"))
            .is_err());
    }
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_parent_and_final_component() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().expect("temporary");
    let root = temporary.path().canonicalize().expect("canonical root");
    fs::create_dir(root.join("real")).expect("real directory");
    fs::write(root.join("real/note.md"), b"note").expect("note");
    symlink(root.join("real"), root.join("linked-parent")).expect("parent symlink");
    symlink(root.join("real/note.md"), root.join("linked.md")).expect("file symlink");
    let vault = Vault::open(&root).expect("vault");

    for path in ["linked-parent/note.md", "linked.md"] {
        assert!(matches!(
            vault.read_note_with_revision(&VaultPath::from_portable(path).expect("path")),
            Err(CoreError::SymlinkRejected(_))
        ));
    }
}
