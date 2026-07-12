#![cfg(any(target_os = "linux", target_os = "macos"))]

use myvault_snapshots::{
    Error, EvidenceLocation, PublishOutcome, SnapshotEvidence, SnapshotManifest, SnapshotRevision,
    SnapshotStore, MAX_PAYLOAD_BYTES,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    _temporary: TempDir,
    app: PathBuf,
    vault: PathBuf,
    vault_id: Uuid,
    store: SnapshotStore,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let base = temporary
            .path()
            .canonicalize()
            .expect("canonical temp root");
        let app = base.join("app");
        let vault = base.join("vault");
        fs::create_dir(&app).expect("app root");
        fs::create_dir(&vault).expect("vault root");
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app root");
        let vault_id = Uuid::new_v4();
        let store = SnapshotStore::open(&app, &vault, vault_id).expect("snapshot store");
        Self {
            _temporary: temporary,
            app,
            vault,
            vault_id,
            store,
        }
    }

    fn manifest(&self, payload: &[u8]) -> SnapshotManifest {
        SnapshotManifest::new(
            Uuid::new_v4(),
            self.vault_id,
            "notes/example.md",
            1_700_000_000_000,
            SnapshotRevision::from_bytes(payload),
        )
        .expect("manifest")
    }

    fn vault_store(&self) -> PathBuf {
        self.app
            .join("recovery-snapshots/v1/vaults")
            .join(self.vault_id.to_string())
    }

    fn object(&self, snapshot_id: Uuid) -> PathBuf {
        self.vault_store()
            .join("objects")
            .join(snapshot_id.to_string())
    }

    fn staging(&self, snapshot_id: Uuid) -> PathBuf {
        self.vault_store()
            .join("staging")
            .join(snapshot_id.to_string())
    }
}

#[test]
fn publishes_exact_immutable_object_and_retries_idempotently() {
    let fixture = Fixture::new();
    let payload = b"# private note\n";
    let manifest = fixture.manifest(payload);

    assert_eq!(
        fixture.store.publish(&manifest, payload).expect("publish"),
        PublishOutcome::Published
    );
    assert_eq!(
        fixture.store.publish(&manifest, payload).expect("retry"),
        PublishOutcome::AlreadyPublished
    );

    let object = fixture.object(manifest.snapshot_id);
    let mut names = fs::read_dir(&object)
        .expect("object entries")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["manifest.json", "payload"]);
    assert_eq!(fs::read(object.join("payload")).expect("payload"), payload);
    assert_eq!(
        fs::metadata(object.join("payload"))
            .expect("payload metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn exact_staging_retry_is_promoted() {
    let fixture = Fixture::new();
    let payload = b"before";
    let manifest = fixture.manifest(payload);
    fixture.store.publish(&manifest, payload).expect("publish");
    fs::rename(
        fixture.object(manifest.snapshot_id),
        fixture.staging(manifest.snapshot_id),
    )
    .expect("simulate crash before promotion");

    assert_eq!(
        fixture.store.publish(&manifest, payload).expect("resume"),
        PublishOutcome::PromotedFromStaging
    );
    assert!(fixture.object(manifest.snapshot_id).is_dir());
    assert!(!fixture.staging(manifest.snapshot_id).exists());
}

#[test]
fn supported_inspection_round_trips_manifest() {
    let fixture = Fixture::new();
    let payload = b"evidence";
    let manifest = fixture.manifest(payload);
    fixture.store.publish(&manifest, payload).expect("publish");

    assert_eq!(
        fixture
            .store
            .inspect(manifest.snapshot_id)
            .expect("inspect"),
        SnapshotEvidence::Supported {
            location: EvidenceLocation::Objects,
            manifest,
        }
    );
}

#[test]
fn unsupported_manifest_is_opaque_and_unchanged() {
    let fixture = Fixture::new();
    let payload = b"future payload";
    let manifest = fixture.manifest(payload);
    fixture.store.publish(&manifest, payload).expect("publish");
    let path = fixture.object(manifest.snapshot_id).join("manifest.json");
    let future = format!(
        "{{\"version\":2,\"snapshot_id\":\"{}\",\"vault_id\":\"{}\",\"future\":{{\"x\":1}}}}",
        manifest.snapshot_id, fixture.vault_id
    )
    .into_bytes();
    fs::write(&path, &future).expect("write future evidence");

    assert!(matches!(
        fixture
            .store
            .inspect(manifest.snapshot_id)
            .expect("inspect"),
        SnapshotEvidence::Unsupported { version: 2, .. }
    ));
    assert_eq!(fs::read(path).expect("future bytes"), future);
}

#[test]
fn both_stable_locations_fail_closed() {
    let fixture = Fixture::new();
    let payload = b"ambiguous";
    let manifest = fixture.manifest(payload);
    fixture.store.publish(&manifest, payload).expect("publish");
    copy_object(
        &fixture.object(manifest.snapshot_id),
        &fixture.staging(manifest.snapshot_id),
    );

    assert!(matches!(
        fixture.store.publish(&manifest, payload),
        Err(Error::AmbiguousEvidence)
    ));
    assert!(matches!(
        fixture.store.inspect(manifest.snapshot_id),
        Err(Error::AmbiguousEvidence)
    ));
}

#[test]
fn mismatched_retry_never_overwrites_existing_object() {
    let fixture = Fixture::new();
    let original = b"original";
    let manifest = fixture.manifest(original);
    fixture.store.publish(&manifest, original).expect("publish");
    let replacement = b"replacement";
    let conflicting = SnapshotManifest::new(
        manifest.snapshot_id,
        fixture.vault_id,
        "notes/other.md",
        manifest.created_at_unix_ms,
        SnapshotRevision::from_bytes(replacement),
    )
    .expect("conflicting manifest");

    assert!(matches!(
        fixture.store.publish(&conflicting, replacement),
        Err(Error::SnapshotCollision)
    ));
    assert_eq!(
        fs::read(fixture.object(manifest.snapshot_id).join("payload")).expect("original payload"),
        original
    );
}

#[test]
fn extra_entry_and_hardlinked_payload_are_rejected() {
    let fixture = Fixture::new();
    let payload = b"topology";
    let first = fixture.manifest(payload);
    fixture.store.publish(&first, payload).expect("publish");
    fs::write(fixture.object(first.snapshot_id).join("extra"), b"junk").expect("extra entry");
    assert!(matches!(
        fixture.store.inspect(first.snapshot_id),
        Err(Error::InvalidObjectTopology)
    ));

    let second = fixture.manifest(payload);
    fixture
        .store
        .publish(&second, payload)
        .expect("publish second");
    let payload_path = fixture.object(second.snapshot_id).join("payload");
    fs::hard_link(&payload_path, fixture.vault_store().join("payload-alias"))
        .expect("hardlink payload");
    assert!(matches!(
        fixture.store.inspect(second.snapshot_id),
        Err(Error::ExternalMutation)
    ));
}

#[test]
fn symlinked_stable_object_is_rejected_without_following() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    let target = fixture.vault_store().join("target");
    fs::create_dir(&target).expect("target");
    symlink(
        &target,
        fixture
            .vault_store()
            .join("objects")
            .join(snapshot_id.to_string()),
    )
    .expect("object symlink");
    assert!(fixture.store.inspect(snapshot_id).is_err());
}

#[test]
fn immutable_binding_rejects_same_id_for_replaced_vault_root() {
    let fixture = Fixture::new();
    let detached = fixture.vault.with_extension("detached");
    fs::rename(&fixture.vault, &detached).expect("detach original vault");
    fs::create_dir(&fixture.vault).expect("replacement vault");

    assert!(matches!(
        SnapshotStore::open(&fixture.app, &fixture.vault, fixture.vault_id),
        Err(Error::BindingCollision)
    ));
}

#[test]
fn manifest_contract_rejects_nil_ids_noncanonical_paths_and_wrong_extension_case() {
    let revision = SnapshotRevision::from_bytes(b"note");
    let vault_id = Uuid::new_v4();
    assert!(matches!(
        SnapshotManifest::new(
            Uuid::nil(),
            vault_id,
            "note.md",
            1_700_000_000_000,
            revision.clone(),
        ),
        Err(Error::InvalidSnapshotId)
    ));
    for path in [
        "../note.md",
        ".trash/note.md",
        ".obsidian/note.md",
        "note.Md",
        "note.txt",
    ] {
        assert!(matches!(
            SnapshotManifest::new(
                Uuid::new_v4(),
                vault_id,
                path,
                1_700_000_000_000,
                revision.clone(),
            ),
            Err(Error::InvalidNotePath)
        ));
    }
}

#[test]
fn payload_bound_and_revision_are_checked_before_publication() {
    let fixture = Fixture::new();
    let manifest = fixture.manifest(b"expected");
    assert!(matches!(
        fixture.store.publish(&manifest, b"different"),
        Err(Error::InvalidRevision)
    ));
    let oversized = vec![0_u8; usize::try_from(MAX_PAYLOAD_BYTES).expect("usize") + 1];
    assert!(matches!(
        fixture.store.publish(&manifest, &oversized),
        Err(Error::PayloadTooLarge)
    ));
    assert!(!fixture.object(manifest.snapshot_id).exists());
}

#[test]
fn current_manifest_denies_unknown_fields_and_noncanonical_uuid_text() {
    let fixture = Fixture::new();
    let payload = b"json";
    let manifest = fixture.manifest(payload);
    let mut value = serde_json::to_value(&manifest).expect("manifest value");
    value
        .as_object_mut()
        .expect("object")
        .insert("unknown".to_owned(), serde_json::Value::Bool(true));
    assert!(serde_json::from_value::<SnapshotManifest>(value).is_err());

    let uppercase = serde_json::to_string(&manifest)
        .expect("manifest json")
        .replace(
            &manifest.snapshot_id.to_string(),
            &manifest.snapshot_id.to_string().to_uppercase(),
        );
    assert!(serde_json::from_str::<SnapshotManifest>(&uppercase).is_err());
}

fn copy_object(source: &Path, destination: &Path) {
    fs::create_dir(destination).expect("destination object");
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
        .expect("private destination");
    for name in ["manifest.json", "payload"] {
        fs::copy(source.join(name), destination.join(name)).expect("copy object file");
        fs::set_permissions(destination.join(name), fs::Permissions::from_mode(0o600))
            .expect("private object file");
    }
}
