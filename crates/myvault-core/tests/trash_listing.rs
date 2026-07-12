use std::fs;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

use myvault_core::{
    CoreError, FileRevision, TrashId, TrashListEvidence, TrashManifestV1, Vault, VaultPath,
    MAX_TRASH_LIST_SCAN, MAX_TRASH_PAGE_SIZE,
};
use uuid::Uuid;

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    vault: Vault,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary");
        let root = temporary.path().canonicalize().expect("canonical root");
        let vault = Vault::open(&root).expect("vault");
        Self {
            _temporary: temporary,
            root,
            vault,
        }
    }

    fn items(&self) -> PathBuf {
        self.root.join(".trash/v1/items")
    }

    fn create_supported(&self, id: &str, original: &str, payload: &[u8]) -> TrashId {
        let id = TrashId::parse(id).expect("trash id");
        let directory = self.items().join(id.to_string());
        fs::create_dir_all(&directory).expect("item directory");
        let manifest = TrashManifestV1::new(
            id,
            Uuid::new_v4(),
            &VaultPath::from_portable(original).expect("original path"),
            FileRevision::from_bytes(payload),
            1_700_000_000_000,
        )
        .expect("manifest");
        fs::write(
            directory.join("manifest.json"),
            manifest.canonical_bytes().expect("canonical manifest"),
        )
        .expect("manifest file");
        fs::write(directory.join("payload"), payload).expect("payload");
        id
    }
}

#[test]
fn missing_hierarchy_is_empty_and_is_not_created() {
    let fixture = Fixture::new();

    let page = fixture
        .vault
        .trash_store()
        .list_items_page(None, 10)
        .expect("empty page");

    assert!(page.entries.is_empty());
    assert!(!fixture.root.join(".trash").exists());
}

#[test]
fn orders_ids_and_pages_with_an_exclusive_cursor() {
    let fixture = Fixture::new();
    let first =
        fixture.create_supported("00000000-0000-4000-8000-000000000001", "first.md", b"first");
    let second = fixture.create_supported(
        "00000000-0000-4000-8000-000000000002",
        "second.md",
        b"second",
    );
    let third =
        fixture.create_supported("00000000-0000-4000-8000-000000000003", "third.md", b"third");

    let page = fixture
        .vault
        .trash_store()
        .list_items_page(None, 2)
        .expect("first page");
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.next_after, Some(second));
    assert!(page.has_more);
    assert!(matches!(
        &page.entries[0],
        TrashListEvidence::Supported { trash_id, .. } if *trash_id == first
    ));

    let page = fixture
        .vault
        .trash_store()
        .list_items_page(page.next_after, 2)
        .expect("second page");
    assert_eq!(page.next_after, Some(third));
    assert!(!page.has_more);
    assert_eq!(page.entries.len(), 1);
}

#[test]
fn invalid_names_are_counted_and_opaque_ids_consume_page_slots() {
    let fixture = Fixture::new();
    let items = fixture.items();
    fs::create_dir_all(&items).expect("items");
    fs::create_dir(items.join("not-a-uuid")).expect("invalid name");
    let opaque = TrashId::parse("00000000-0000-4000-8000-000000000001").expect("id");
    let opaque_dir = items.join(opaque.to_string());
    fs::create_dir(&opaque_dir).expect("opaque directory");
    fs::write(opaque_dir.join("manifest.json"), b"{\"version\":2}").expect("future manifest");
    fs::write(opaque_dir.join("payload"), b"future").expect("future payload");
    let supported =
        fixture.create_supported("00000000-0000-4000-8000-000000000002", "note.md", b"note");

    let first = fixture
        .vault
        .trash_store()
        .list_items_page(None, 1)
        .expect("page");
    assert_eq!(first.invalid_name_count, 1);
    assert_eq!(
        first.entries,
        vec![TrashListEvidence::Opaque { trash_id: opaque }]
    );
    assert!(first.has_more);
    assert_eq!(
        fs::read(opaque_dir.join("manifest.json")).expect("future bytes"),
        b"{\"version\":2}"
    );

    let second = fixture
        .vault
        .trash_store()
        .list_items_page(first.next_after, 1)
        .expect("next page");
    assert!(matches!(
        &second.entries[0],
        TrashListEvidence::Supported { trash_id, .. } if *trash_id == supported
    ));
}

#[test]
fn listing_does_not_hash_payload_content() {
    let fixture = Fixture::new();
    let id = fixture.create_supported(
        "00000000-0000-4000-8000-000000000001",
        "note.md",
        b"expected",
    );
    fs::write(
        fixture.items().join(id.to_string()).join("payload"),
        b"mutation",
    )
    .expect("same-length mutation");

    let page = fixture
        .vault
        .trash_store()
        .list_items_page(None, 1)
        .expect("page");

    assert!(matches!(
        &page.entries[0],
        TrashListEvidence::Supported { trash_id, .. } if *trash_id == id
    ));
}

#[test]
fn unsafe_or_incomplete_item_topologies_are_opaque() {
    let fixture = Fixture::new();
    let missing = fixture.create_supported(
        "00000000-0000-4000-8000-000000000001",
        "missing.md",
        b"missing",
    );
    fs::remove_file(fixture.items().join(missing.to_string()).join("payload"))
        .expect("remove payload");
    let extra =
        fixture.create_supported("00000000-0000-4000-8000-000000000002", "extra.md", b"extra");
    fs::write(
        fixture.items().join(extra.to_string()).join("extra"),
        b"extra",
    )
    .expect("extra entry");
    let wrong_length = fixture.create_supported(
        "00000000-0000-4000-8000-000000000003",
        "length.md",
        b"length",
    );
    fs::write(
        fixture
            .items()
            .join(wrong_length.to_string())
            .join("payload"),
        b"longer length",
    )
    .expect("wrong length");
    let hardlinked = fixture.create_supported(
        "00000000-0000-4000-8000-000000000004",
        "hardlinked.md",
        b"hardlinked",
    );
    fs::hard_link(
        fixture.items().join(hardlinked.to_string()).join("payload"),
        fixture.root.join("payload-alias"),
    )
    .expect("payload hard link");
    let noncanonical = fixture.create_supported(
        "00000000-0000-4000-8000-000000000005",
        "noncanonical.md",
        b"noncanonical",
    );
    let manifest_path = fixture
        .items()
        .join(noncanonical.to_string())
        .join("manifest.json");
    let mut noncanonical_bytes = fs::read(&manifest_path).expect("manifest bytes");
    noncanonical_bytes.push(b'\n');
    fs::write(manifest_path, noncanonical_bytes).expect("noncanonical manifest");

    let page = fixture
        .vault
        .trash_store()
        .list_items_page(None, 10)
        .expect("page");
    assert_eq!(page.entries.len(), 5);
    assert!(page
        .entries
        .iter()
        .all(|entry| matches!(entry, TrashListEvidence::Opaque { .. })));
}

#[test]
fn rejects_invalid_page_sizes_and_excessive_physical_inventory() {
    let fixture = Fixture::new();
    for limit in [0, MAX_TRASH_PAGE_SIZE + 1] {
        assert!(matches!(
            fixture.vault.trash_store().list_items_page(None, limit),
            Err(CoreError::ResourceLimitExceeded { .. })
        ));
    }

    fs::create_dir_all(fixture.items()).expect("items");
    for index in 0..MAX_TRASH_LIST_SCAN {
        fs::write(fixture.items().join(format!("invalid-{index}")), []).expect("entry");
    }
    let bounded = fixture
        .vault
        .trash_store()
        .list_items_page(None, 1)
        .expect("exact scan bound");
    assert_eq!(bounded.scanned_entries, MAX_TRASH_LIST_SCAN);
    assert_eq!(bounded.invalid_name_count, MAX_TRASH_LIST_SCAN);
    fs::write(fixture.items().join("one-too-many"), []).expect("extra entry");
    assert!(matches!(
        fixture.vault.trash_store().list_items_page(None, 1),
        Err(CoreError::ResourceLimitExceeded { .. })
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_hierarchy_and_payload_are_never_followed() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).expect("outside");
    symlink(&outside, fixture.root.join(".trash")).expect("trash symlink");
    assert!(fixture
        .vault
        .trash_store()
        .list_items_page(None, 10)
        .is_err());

    fs::remove_file(fixture.root.join(".trash")).expect("remove symlink");
    let id = fixture.create_supported("00000000-0000-4000-8000-000000000001", "note.md", b"note");
    let payload = fixture.items().join(id.to_string()).join("payload");
    fs::remove_file(&payload).expect("remove payload");
    symlink(&outside, payload).expect("payload symlink");
    let page = fixture
        .vault
        .trash_store()
        .list_items_page(None, 10)
        .expect("opaque item");
    assert_eq!(
        page.entries,
        vec![TrashListEvidence::Opaque { trash_id: id }]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf_name_is_counted_and_symlink_payload_is_opaque() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fs::create_dir_all(fixture.items()).expect("items");
    fs::create_dir(fixture.items().join(OsString::from_vec(vec![0xff]))).expect("non-UTF name");
    let id = fixture.create_supported("00000000-0000-4000-8000-000000000001", "note.md", b"note");
    let payload = fixture.items().join(id.to_string()).join("payload");
    fs::remove_file(&payload).expect("remove payload");
    symlink(Path::new("outside"), payload).expect("payload symlink");

    let page = fixture
        .vault
        .trash_store()
        .list_items_page(None, 10)
        .expect("page");
    assert_eq!(page.invalid_name_count, 1);
    assert_eq!(
        page.entries,
        vec![TrashListEvidence::Opaque { trash_id: id }]
    );
}
