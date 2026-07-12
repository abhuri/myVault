#![cfg(any(target_os = "linux", target_os = "macos"))]

use myvault_snapshots::{
    Error, RetentionPolicy, RetentionReason, SnapshotManifest, SnapshotRevision, SnapshotStore,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{mpsc, Arc};
use std::time::Duration;
use uuid::Uuid;

struct Fixture {
    _temporary: tempfile::TempDir,
    app: std::path::PathBuf,
    vault: std::path::PathBuf,
    vault_id: Uuid,
    store: Arc<SnapshotStore>,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let base = temporary.path().canonicalize().expect("canonical root");
        let app = base.join("app");
        let vault = base.join("vault");
        fs::create_dir(&app).expect("app");
        fs::create_dir(&vault).expect("vault");
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app");
        let vault_id = Uuid::new_v4();
        let store = Arc::new(SnapshotStore::open(&app, &vault, vault_id).expect("store"));
        Self {
            _temporary: temporary,
            app,
            vault,
            vault_id,
            store,
        }
    }

    fn root(&self) -> std::path::PathBuf {
        self.app
            .join("recovery-snapshots/v1/vaults")
            .join(self.vault_id.to_string())
    }

    fn publish(&self, path: &str, timestamp: u64, payload: &[u8]) -> SnapshotManifest {
        let manifest = SnapshotManifest::new(
            Uuid::new_v4(),
            self.vault_id,
            path,
            timestamp,
            SnapshotRevision::from_bytes(payload),
        )
        .expect("manifest");
        self.store.publish(&manifest, payload).expect("publish");
        manifest
    }
}

#[test]
fn default_policy_is_thirty_days_one_hundred_and_one_gibibyte() {
    let policy = RetentionPolicy::default();
    assert_eq!(policy.max_age_ms, 30 * 24 * 60 * 60 * 1000);
    assert_eq!(policy.max_per_lineage, 100);
    assert_eq!(policy.max_logical_bytes, 1024 * 1024 * 1024);
}

#[test]
fn pure_policy_unions_reasons_and_orders_by_timestamp_then_uuid() {
    let vault_id = Uuid::new_v4();
    let first_id = Uuid::parse_str("00000000-0000-4000-8000-000000000001").expect("uuid");
    let second_id = Uuid::parse_str("00000000-0000-4000-8000-000000000002").expect("uuid");
    let manifests = [
        manifest(first_id, vault_id, "Note.md", 10, 7),
        manifest(second_id, vault_id, "note.MD", 10, 7),
        manifest(Uuid::new_v4(), vault_id, "renamed.md", 20, 7),
    ];
    let policy = RetentionPolicy {
        max_age_ms: 5,
        max_per_lineage: 1,
        max_logical_bytes: 7,
    };
    let candidates = policy.plan_manifests(30, &manifests).expect("plan");

    assert_eq!(candidates[0].snapshot_id, first_id);
    assert_eq!(
        candidates[0].reasons,
        [
            RetentionReason::Age,
            RetentionReason::LineageCount,
            RetentionReason::TotalSize
        ]
    );
    assert_eq!(candidates[1].snapshot_id, second_id);
    assert_eq!(
        candidates[1].reasons,
        [RetentionReason::Age, RetentionReason::TotalSize]
    );
    assert_eq!(candidates[2].path, "renamed.md");
    assert_eq!(
        candidates[2].reasons,
        [RetentionReason::Age, RetentionReason::TotalSize]
    );
}

#[test]
fn exact_age_count_and_size_boundaries_are_kept_until_exceeded() {
    let vault_id = Uuid::new_v4();
    let thirty_days = RetentionPolicy::default().max_age_ms;
    let epoch_zero = manifest(Uuid::from_u128(2), vault_id, "epoch.md", 0, 1);
    assert!(RetentionPolicy::default()
        .plan_manifests(thirty_days - 1, &[epoch_zero])
        .expect("clock has not reached cutoff")
        .is_empty());
    let inside_boundary = manifest(Uuid::from_u128(1), vault_id, "age.md", 11, 1);
    assert!(RetentionPolicy::default()
        .plan_manifests(10 + thirty_days, &[inside_boundary])
        .expect("age boundary")
        .is_empty());
    let at_cutoff = manifest(Uuid::from_u128(1), vault_id, "age.md", 10, 1);
    assert_eq!(
        RetentionPolicy::default()
            .plan_manifests(10 + thirty_days, &[at_cutoff])
            .expect("exact cutoff")
            .len(),
        1
    );

    let one_hundred = (1_u128..=100)
        .map(|id| {
            manifest(
                Uuid::from_u128(id),
                vault_id,
                "count.md",
                u64::try_from(id).expect("small id"),
                1,
            )
        })
        .collect::<Vec<_>>();
    assert!(RetentionPolicy::default()
        .plan_manifests(100, &one_hundred)
        .expect("count boundary")
        .is_empty());
    let mut one_hundred_one = one_hundred;
    one_hundred_one.push(manifest(Uuid::from_u128(101), vault_id, "count.md", 101, 1));
    let count_plan = RetentionPolicy::default()
        .plan_manifests(101, &one_hundred_one)
        .expect("count exceeded");
    assert_eq!(count_plan.len(), 1);
    assert_eq!(count_plan[0].snapshot_id, Uuid::from_u128(1));

    let mut one_gib = (1_u128..=64)
        .map(|id| {
            manifest_with_len(
                Uuid::from_u128(id),
                vault_id,
                u64::try_from(id).expect("small id"),
                16 * 1024 * 1024 - 4096,
            )
        })
        .collect::<Vec<_>>();
    let size_only = RetentionPolicy {
        max_age_ms: u64::MAX,
        max_per_lineage: usize::MAX,
        max_logical_bytes: 1024 * 1024 * 1024,
    };
    let base = one_gib.iter().map(logical_len).sum::<u64>();
    let mut final_payload = 1024 * 1024 * 1024 - base;
    loop {
        let final_manifest = manifest_with_len(Uuid::from_u128(65), vault_id, 65, final_payload);
        let total = base + logical_len(&final_manifest);
        if total == 1024 * 1024 * 1024 {
            one_gib.push(final_manifest);
            break;
        }
        final_payload = if total < 1024 * 1024 * 1024 {
            final_payload + (1024 * 1024 * 1024 - total)
        } else {
            final_payload - (total - 1024 * 1024 * 1024)
        };
    }
    assert!(size_only
        .plan_manifests(65, &one_gib)
        .expect("size boundary")
        .is_empty());
    let mut over = one_gib;
    over.last_mut().expect("last").revision.byte_len += 1;
    assert_eq!(
        size_only
            .plan_manifests(65, &over)
            .expect("size exceeded")
            .len(),
        1
    );
}

#[test]
fn pure_planner_rejects_duplicate_ids_and_mixed_vaults() {
    let first_vault = Uuid::new_v4();
    let second_vault = Uuid::new_v4();
    let id = Uuid::new_v4();
    let duplicate = [
        manifest(id, first_vault, "a.md", 1, 1),
        manifest(id, first_vault, "b.md", 2, 1),
    ];
    assert!(matches!(
        RetentionPolicy::default().plan_manifests(0, &duplicate),
        Err(Error::SnapshotCollision)
    ));
    let mixed = [
        manifest(Uuid::new_v4(), first_vault, "a.md", 1, 1),
        manifest(Uuid::new_v4(), second_vault, "b.md", 2, 1),
    ];
    assert!(matches!(
        RetentionPolicy::default().plan_manifests(0, &mixed),
        Err(Error::SnapshotCollision)
    ));
}

#[test]
fn pure_plan_is_independent_of_insertion_order() {
    let vault_id = Uuid::new_v4();
    let policy = RetentionPolicy {
        max_age_ms: 0,
        max_per_lineage: 2,
        max_logical_bytes: 10,
    };
    let mut manifests = (1_u128..=6)
        .map(|id| {
            manifest(
                Uuid::from_u128(id),
                vault_id,
                "note.md",
                u64::try_from(id).expect("small id"),
                4,
            )
        })
        .collect::<Vec<_>>();
    let forward = policy.plan_manifests(10, &manifests).expect("forward");
    manifests.reverse();
    let reverse = policy.plan_manifests(10, &manifests).expect("reverse");
    assert_eq!(forward, reverse);
}

#[test]
fn dry_run_inventory_is_deterministic_and_never_modifies_objects() {
    let fixture = Fixture::new();
    let old = fixture.publish("note.md", 1, b"old");
    let recent = fixture.publish("note.md", 100, b"recent");
    let before = fs::read_dir(fixture.root().join("objects"))
        .expect("objects")
        .count();
    let policy = RetentionPolicy {
        max_age_ms: 50,
        max_per_lineage: 100,
        max_logical_bytes: u64::MAX,
    };

    let first = fixture.store.plan_retention(100, policy).expect("plan");
    let second = fixture.store.plan_retention(100, policy).expect("repeat");
    assert_eq!(first, second);
    assert_eq!(first.candidates.len(), 1);
    assert_eq!(first.candidates[0].snapshot_id, old.snapshot_id);
    assert_eq!(first.candidates[0].logical_bytes, logical_len(&old));
    assert_eq!(first.supported_objects, 2);
    assert_eq!(
        first.supported_logical_bytes,
        logical_len(&old) + logical_len(&recent)
    );
    assert!(first.capacity_proven);
    assert_eq!(
        fs::read_dir(fixture.root().join("objects"))
            .expect("objects after")
            .count(),
        before
    );
    assert!(fixture
        .root()
        .join("objects")
        .join(recent.snapshot_id.to_string())
        .exists());
}

#[test]
fn future_evidence_and_work_directories_are_opaque_and_never_candidates() {
    let fixture = Fixture::new();
    let manifest = fixture.publish("future.md", 1, b"future");
    let object = fixture
        .root()
        .join("objects")
        .join(manifest.snapshot_id.to_string());
    let future = format!(
        "{{\"version\":2,\"snapshot_id\":\"{}\",\"vault_id\":\"{}\"}}",
        manifest.snapshot_id, fixture.vault_id
    );
    fs::write(object.join("manifest.json"), &future).expect("future manifest");
    fs::create_dir(fixture.root().join("staging/.work-opaque")).expect("work evidence");

    let plan = fixture
        .store
        .plan_retention(u64::MAX, RetentionPolicy::default())
        .expect("plan");
    assert!(plan.candidates.is_empty());
    assert_eq!(plan.opaque_evidence, 2);
    assert_eq!(
        plan.verified_bytes,
        u64::try_from(future.len()).expect("future length")
            + u64::try_from(b"future".len()).expect("payload length")
    );
    assert!(!plan.capacity_proven);
}

#[test]
fn malformed_bytes_are_charged_even_when_parsing_fails() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    write_raw_object(
        &fixture.root().join("objects"),
        snapshot_id,
        b"{",
        b"payload",
    );
    let plan = fixture
        .store
        .plan_retention(0, RetentionPolicy::default())
        .expect("plan");
    assert_eq!(plan.opaque_evidence, 1);
    assert_eq!(plan.verified_bytes, 1 + 7);
    assert!(plan.candidates.is_empty());
}

#[test]
fn retention_waits_for_the_same_cross_process_lock() {
    let fixture = Fixture::new();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.root().join("operation.lock"))
        .expect("lock file");
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).expect("external lock");
    let store = Arc::clone(&fixture.store);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send(store.plan_retention(0, RetentionPolicy::default()))
            .expect("send result");
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::Unlock).expect("unlock");
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("planner resumed")
        .expect("plan");
    worker.join().expect("worker");
}

#[test]
fn two_store_instances_use_the_same_named_lock() {
    let fixture = Fixture::new();
    let second = Arc::new(
        SnapshotStore::open(&fixture.app, &fixture.vault, fixture.vault_id).expect("second store"),
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.root().join("operation.lock"))
        .expect("lock file");
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).expect("external lock");
    let (sender, receiver) = mpsc::channel();
    for store in [Arc::clone(&fixture.store), second] {
        let sender = sender.clone();
        std::thread::spawn(move || {
            sender
                .send(store.plan_retention(0, RetentionPolicy::default()))
                .expect("send");
        });
    }
    drop(sender);
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::Unlock).expect("unlock");
    for _ in 0..2 {
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("store resumed")
            .expect("plan");
    }
}

#[test]
fn hardlinked_lock_evidence_fails_closed() {
    let fixture = Fixture::new();
    fs::hard_link(
        fixture.root().join("operation.lock"),
        fixture.root().join("lock-alias"),
    )
    .expect("hardlink lock");
    assert!(matches!(
        fixture.store.plan_retention(0, RetentionPolicy::default()),
        Err(Error::ExternalMutation)
    ));
}

#[test]
fn named_lock_replacement_after_open_fails_closed() {
    let fixture = Fixture::new();
    let original = fixture.root().join("operation.lock");
    fs::rename(&original, fixture.root().join("detached-lock")).expect("detach lock");
    fs::write(&original, b"").expect("replacement lock");
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600)).expect("private replacement");

    assert!(matches!(
        fixture.store.plan_retention(0, RetentionPolicy::default()),
        Err(Error::ExternalMutation)
    ));
}

#[test]
fn candidate_cap_is_reported_without_selecting_more_than_256() {
    let fixture = Fixture::new();
    let objects = fixture.root().join("objects");
    for id in 1_u128..=257 {
        let manifest = manifest(Uuid::from_u128(id), fixture.vault_id, "cap.md", 0, 1);
        write_object(&objects, &manifest, b"\0");
    }
    let plan = fixture
        .store
        .plan_retention(
            1,
            RetentionPolicy {
                max_age_ms: 0,
                max_per_lineage: usize::MAX,
                max_logical_bytes: u64::MAX,
            },
        )
        .expect("bounded plan");
    assert_eq!(plan.candidates.len(), 256);
    assert!(plan.candidate_cap_reached);
    assert!(!plan.capacity_proven);
}

#[test]
fn physical_scan_fails_closed_above_8192_entries() {
    let fixture = Fixture::new();
    let staging = fixture.root().join("staging");
    for index in 0..=8192 {
        fs::write(staging.join(format!("junk-{index}")), b"").expect("junk evidence");
    }
    assert!(matches!(
        fixture.store.plan_retention(0, RetentionPolicy::default()),
        Err(Error::TooManySnapshotEntries)
    ));
}

fn manifest(
    snapshot_id: Uuid,
    vault_id: Uuid,
    path: &str,
    timestamp: u64,
    byte_len: usize,
) -> SnapshotManifest {
    SnapshotManifest::new(
        snapshot_id,
        vault_id,
        path,
        timestamp,
        SnapshotRevision::from_bytes(&vec![0; byte_len]),
    )
    .expect("manifest")
}

fn manifest_with_len(
    snapshot_id: Uuid,
    vault_id: Uuid,
    timestamp: u64,
    byte_len: u64,
) -> SnapshotManifest {
    SnapshotManifest::new(
        snapshot_id,
        vault_id,
        "size.md",
        timestamp,
        SnapshotRevision {
            blake3_hex: "0".repeat(64),
            byte_len,
        },
    )
    .expect("manifest")
}

fn write_object(parent: &std::path::Path, manifest: &SnapshotManifest, payload: &[u8]) {
    let object = parent.join(manifest.snapshot_id.to_string());
    fs::create_dir(&object).expect("object");
    fs::set_permissions(&object, fs::Permissions::from_mode(0o700)).expect("private object");
    fs::write(
        object.join("manifest.json"),
        serde_json::to_vec(manifest).expect("manifest json"),
    )
    .expect("manifest file");
    fs::write(object.join("payload"), payload).expect("payload file");
    for name in ["manifest.json", "payload"] {
        fs::set_permissions(object.join(name), fs::Permissions::from_mode(0o600))
            .expect("private file");
    }
}

fn write_raw_object(parent: &std::path::Path, snapshot_id: Uuid, manifest: &[u8], payload: &[u8]) {
    let object = parent.join(snapshot_id.to_string());
    fs::create_dir(&object).expect("object");
    fs::set_permissions(&object, fs::Permissions::from_mode(0o700)).expect("private object");
    fs::write(object.join("manifest.json"), manifest).expect("manifest");
    fs::write(object.join("payload"), payload).expect("payload");
    for name in ["manifest.json", "payload"] {
        fs::set_permissions(object.join(name), fs::Permissions::from_mode(0o600))
            .expect("private file");
    }
}

fn logical_len(manifest: &SnapshotManifest) -> u64 {
    u64::try_from(
        serde_json::to_vec(manifest)
            .expect("canonical manifest")
            .len(),
    )
    .expect("manifest length")
        + manifest.revision.byte_len
}
