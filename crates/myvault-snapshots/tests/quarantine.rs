#![cfg(any(target_os = "linux", target_os = "macos"))]

use myvault_snapshots::{
    Error, GcPlan, QuarantineOutcome, RetentionPolicy, SnapshotManifest, SnapshotRevision,
    SnapshotStore,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

struct Fixture {
    _temporary: tempfile::TempDir,
    app: PathBuf,
    vault_id: Uuid,
    store: SnapshotStore,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary");
        let base = temporary.path().canonicalize().expect("canonical root");
        let app = base.join("app");
        let vault = base.join("vault");
        fs::create_dir(&app).expect("app");
        fs::create_dir(&vault).expect("vault");
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app");
        let vault_id = Uuid::new_v4();
        let store = SnapshotStore::open(&app, &vault, vault_id).expect("store");
        Self {
            _temporary: temporary,
            app,
            vault_id,
            store,
        }
    }

    fn root(&self) -> PathBuf {
        self.app
            .join("recovery-snapshots/v1/vaults")
            .join(self.vault_id.to_string())
    }

    fn run(&self, run_id: Uuid) -> PathBuf {
        self.root()
            .join("quarantine/v1/runs")
            .join(run_id.to_string())
    }

    fn publish(&self, snapshot_id: Uuid, payload: &[u8]) -> SnapshotManifest {
        let manifest = SnapshotManifest::new(
            snapshot_id,
            self.vault_id,
            "note.md",
            0,
            SnapshotRevision::from_bytes(payload),
        )
        .expect("manifest");
        self.store.publish(&manifest, payload).expect("publish");
        manifest
    }
}

fn aggressive_policy() -> RetentionPolicy {
    RetentionPolicy {
        max_age_ms: 0,
        max_per_lineage: usize::MAX,
        max_logical_bytes: u64::MAX,
    }
}

#[test]
fn creates_canonical_plan_detaches_marks_and_is_idempotent() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    let payload = b"snapshot";
    let manifest = fixture.publish(snapshot_id, payload);
    let run_id = Uuid::new_v4();

    let report = fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    assert_eq!(report.outcome, QuarantineOutcome::Created);
    assert_eq!(report.detached, 1);
    assert!(!fixture
        .root()
        .join("objects")
        .join(snapshot_id.to_string())
        .exists());
    assert!(fixture
        .run(run_id)
        .join("items")
        .join(snapshot_id.to_string())
        .is_dir());
    assert!(fixture
        .run(run_id)
        .join("state")
        .join(format!("{snapshot_id}.json"))
        .is_file());

    let plan_bytes = fs::read(fixture.run(run_id).join("plan.json")).expect("plan");
    assert!(plan_bytes.len() <= 128 * 1024);
    let plan: GcPlan = serde_json::from_slice(&plan_bytes).expect("plan json");
    assert_eq!(plan.run_id, run_id);
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.candidates[0].snapshot_id, snapshot_id);
    assert_eq!(
        plan.candidates[0].payload_blake3,
        manifest.revision.blake3_hex
    );

    let retry = fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("retry");
    assert_eq!(retry.outcome, QuarantineOutcome::RecoveredExisting);
    assert_eq!(retry.already_marked, 1);

    fixture
        .store
        .publish(&manifest, payload)
        .expect("republish same id");
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("marker-safe retry");
    assert!(fixture
        .root()
        .join("objects")
        .join(snapshot_id.to_string())
        .is_dir());
}

#[test]
fn destination_only_crash_state_finalizes_marker() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    fs::remove_file(
        fixture
            .run(run_id)
            .join("state")
            .join(format!("{snapshot_id}.json")),
    )
    .expect("simulate crash before marker");
    let marker_staging = fixture.run(run_id).join("marker-staging");
    let partial = marker_staging.join(format!("{snapshot_id}.{}.tmp", Uuid::new_v4()));
    fs::write(&partial, b"partial").expect("partial staged marker");
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o600)).expect("private partial");
    assert!(matches!(
        fixture
            .store
            .quarantine_retention(run_id, 2, aggressive_policy()),
        Err(Error::QuarantineCollision)
    ));
    assert!(fixture
        .run(run_id)
        .join("items")
        .join(snapshot_id.to_string())
        .is_dir());
    assert!(!fixture
        .run(run_id)
        .join("state")
        .join(format!("{snapshot_id}.json"))
        .exists());

    let report = fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("recover destination-only");
    assert_eq!(report.detached, 0);
    assert!(fixture
        .run(run_id)
        .join("state")
        .join(format!("{snapshot_id}.json"))
        .is_file());
}

#[test]
fn both_locations_and_extra_evidence_fail_closed() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    fs::remove_file(
        fixture
            .run(run_id)
            .join("state")
            .join(format!("{snapshot_id}.json")),
    )
    .expect("remove marker");
    copy_object(
        &fixture
            .run(run_id)
            .join("items")
            .join(snapshot_id.to_string()),
        &fixture.root().join("objects").join(snapshot_id.to_string()),
    );
    assert!(matches!(
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy()),
        Err(Error::QuarantineCollision)
    ));

    fs::write(fixture.run(run_id).join("state/extra"), b"").expect("extra evidence");
    assert!(fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .is_err());
}

#[test]
fn malformed_plan_is_opaque_and_blocks_recovery() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    fs::write(fixture.run(run_id).join("plan.json"), b"{}").expect("corrupt plan");
    assert!(matches!(
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy()),
        Err(Error::InvalidGcPlan)
    ));
}

#[test]
fn marker_attempt_exhaustion_and_unknown_candidate_fail_without_mutation() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    let marker = fixture
        .run(run_id)
        .join("state")
        .join(format!("{snapshot_id}.json"));
    fs::remove_file(&marker).expect("remove marker");
    let staging = fixture.run(run_id).join("marker-staging");
    for _ in 0..4 {
        let attempt = staging.join(format!("{snapshot_id}.{}.tmp", Uuid::new_v4()));
        fs::write(&attempt, b"partial").expect("partial");
        fs::set_permissions(&attempt, fs::Permissions::from_mode(0o600)).expect("private");
    }
    assert!(matches!(
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy()),
        Err(Error::DetachedOutcomeUnknown { .. } | Error::QuarantineCollision)
    ));
    assert_eq!(fs::read_dir(&staging).expect("staging").count(), 4);
    assert!(!marker.exists());
    assert!(fixture
        .run(run_id)
        .join("items")
        .join(snapshot_id.to_string())
        .is_dir());

    let unknown = staging.join(format!("{}.{}.tmp", Uuid::new_v4(), Uuid::new_v4()));
    fs::write(&unknown, b"partial").expect("unknown");
    fs::set_permissions(&unknown, fs::Permissions::from_mode(0o600)).expect("private unknown");
    assert!(fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .is_err());
    assert!(!marker.exists());
}

#[test]
fn no_candidates_creates_no_stable_run_and_retry_request_must_match() {
    let fixture = Fixture::new();
    let empty_run = Uuid::new_v4();
    let report = fixture
        .store
        .quarantine_retention(empty_run, 0, RetentionPolicy::default())
        .expect("no candidates");
    assert_eq!(report.outcome, QuarantineOutcome::NoCandidates);
    assert!(!fixture.run(empty_run).exists());

    fixture.publish(Uuid::new_v4(), b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("created run");
    assert!(matches!(
        fixture
            .store
            .quarantine_retention(run_id, 2, aggressive_policy()),
        Err(Error::QuarantineCollision)
    ));
}

#[test]
fn stale_work_is_preserved_reported_and_does_not_block_stable_recovery() {
    let fixture = Fixture::new();
    fixture
        .store
        .quarantine_retention(Uuid::new_v4(), 0, RetentionPolicy::default())
        .expect("create roots only");
    let stale = fixture.root().join("quarantine/v1/work/.work-stale");
    fs::create_dir(&stale).expect("stale work");
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o700)).expect("private stale");
    fixture.publish(Uuid::new_v4(), b"payload");
    let run_id = Uuid::new_v4();
    let report = fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("run despite stale work");
    assert_eq!(report.stale_work_entries, 1);
    assert!(stale.exists());
    let retry = fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("recover stable run");
    assert_eq!(retry.outcome, QuarantineOutcome::RecoveredExisting);
}

fn copy_object(source: &Path, destination: &Path) {
    fs::create_dir(destination).expect("destination");
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700)).expect("private dir");
    for name in ["manifest.json", "payload"] {
        fs::copy(source.join(name), destination.join(name)).expect("copy");
        fs::set_permissions(destination.join(name), fs::Permissions::from_mode(0o600))
            .expect("private file");
    }
}
