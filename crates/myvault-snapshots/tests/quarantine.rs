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
fn marker_authorized_delete_reclaims_item_and_never_touches_republished_object() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    let payload = b"original snapshot";
    let manifest = fixture.publish(snapshot_id, payload);
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    fixture
        .store
        .publish(&manifest, payload)
        .expect("republish same id");
    let object = fixture.root().join("objects").join(snapshot_id.to_string());
    let before_manifest = fs::read(object.join("manifest.json")).expect("object manifest");
    let before_payload = fs::read(object.join("payload")).expect("object payload");
    let expected_reclaimed = plan_logical_bytes(&fixture, run_id, snapshot_id);

    let report = fixture
        .store
        .delete_quarantined_run(run_id)
        .expect("delete quarantine run");
    assert_eq!(report.reclaimed_bytes, expected_reclaimed);
    assert!(!fixture.run(run_id).exists());
    assert_eq!(
        fs::read(object.join("manifest.json")).expect("manifest unchanged"),
        before_manifest
    );
    assert_eq!(
        fs::read(object.join("payload")).expect("payload unchanged"),
        before_payload
    );
}

#[test]
fn deletion_recovers_manifest_only_empty_and_absent_item_states() {
    for state in 1_u8..=3 {
        let fixture = Fixture::new();
        let snapshot_id = Uuid::new_v4();
        fixture.publish(snapshot_id, b"payload");
        let run_id = Uuid::new_v4();
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy())
            .expect("quarantine");
        let item = fixture
            .run(run_id)
            .join("items")
            .join(snapshot_id.to_string());
        fs::remove_file(item.join("payload")).expect("payload crash boundary");
        if state >= 2 {
            fs::remove_file(item.join("manifest.json")).expect("manifest crash boundary");
        }
        if state == 3 {
            fs::remove_dir(&item).expect("item absent boundary");
        }
        fixture
            .store
            .delete_quarantined_run(run_id)
            .expect("resume deletion");
        assert!(!fixture.run(run_id).exists());
    }
}

#[test]
fn corrupt_manifest_only_item_is_preserved_and_rejected() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    let item = fixture
        .run(run_id)
        .join("items")
        .join(snapshot_id.to_string());
    fs::remove_file(item.join("payload")).expect("payload removed");
    fs::write(item.join("manifest.json"), b"{}").expect("corrupt manifest");
    assert!(fixture.store.delete_quarantined_run(run_id).is_err());
    assert!(item.join("manifest.json").exists());
}

#[test]
fn completed_cleanup_recovers_missing_control_directories_and_plan() {
    for phase in 0_u8..=2 {
        let fixture = Fixture::new();
        let snapshot_id = Uuid::new_v4();
        fixture.publish(snapshot_id, b"payload");
        let run_id = Uuid::new_v4();
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy())
            .expect("quarantine");
        let run = fixture.run(run_id);
        let plan_bytes = fs::read(run.join("plan.json")).expect("plan");
        let plan: GcPlan = serde_json::from_slice(&plan_bytes).expect("plan json");
        let item = run.join("items").join(snapshot_id.to_string());
        fs::remove_file(item.join("payload")).expect("payload");
        fs::remove_file(item.join("manifest.json")).expect("manifest");
        fs::remove_dir(&item).expect("item");
        let complete = format!(
            "{{\"version\":1,\"run_id\":\"{run_id}\",\"vault_id\":\"{}\",\"plan_blake3\":\"{}\"}}",
            fixture.vault_id, plan.plan_blake3
        );
        fs::write(run.join("complete.json"), complete).expect("complete");
        fs::set_permissions(run.join("complete.json"), fs::Permissions::from_mode(0o600))
            .expect("private complete");
        if phase >= 1 {
            fs::remove_file(run.join("state").join(format!("{snapshot_id}.json")))
                .expect("state marker");
            fs::remove_dir(run.join("state")).expect("state");
            fs::remove_dir(run.join("marker-staging")).expect("marker staging");
            fs::remove_dir(run.join("items")).expect("items");
        }
        if phase == 2 {
            fs::remove_file(run.join("plan.json")).expect("plan removed");
        }
        fixture
            .store
            .delete_quarantined_run(run_id)
            .expect("resume completed cleanup");
        assert!(!run.exists());
    }
}

#[test]
fn extra_state_marker_blocks_completion_and_is_preserved() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    let run = fixture.run(run_id);
    let extra = run.join("state").join(format!("{}.json", Uuid::new_v4()));
    let extra_bytes =
        fs::read(run.join("state").join(format!("{snapshot_id}.json"))).expect("planned marker");
    fs::write(&extra, &extra_bytes).expect("extra marker");
    fs::set_permissions(&extra, fs::Permissions::from_mode(0o600)).expect("private extra");

    assert!(matches!(
        fixture.store.delete_quarantined_run(run_id),
        Err(Error::QuarantineCollision)
    ));
    assert!(!run.join("complete.json").exists());
    assert_eq!(fs::read(&extra).expect("extra preserved"), extra_bytes);
}

#[test]
fn mismatched_completion_attempt_blocks_cleanup_and_is_preserved() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    let run = fixture.run(run_id);
    prepare_completed_run(&fixture, run_id, snapshot_id);
    let attempt = run.join(format!(".complete-{}.tmp", Uuid::new_v4()));
    let mismatched = b"opaque mismatched completion attempt";
    fs::write(&attempt, mismatched).expect("attempt");
    fs::set_permissions(&attempt, fs::Permissions::from_mode(0o600)).expect("private attempt");

    assert!(matches!(
        fixture.store.delete_quarantined_run(run_id),
        Err(Error::QuarantineCollision)
    ));
    assert_eq!(fs::read(&attempt).expect("attempt preserved"), mismatched);
    assert!(run.join("complete.json").exists());
}

#[test]
fn completion_without_plan_rejects_and_preserves_existing_state() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    let run = fixture.run(run_id);
    prepare_completed_run(&fixture, run_id, snapshot_id);
    fs::remove_file(run.join("plan.json")).expect("remove plan");
    let state = run.join("state").join(format!("{snapshot_id}.json"));
    let before = fs::read(&state).expect("state before");

    assert!(matches!(
        fixture.store.delete_quarantined_run(run_id),
        Err(Error::QuarantineCollision)
    ));
    assert_eq!(fs::read(&state).expect("state preserved"), before);
    assert!(run.join("complete.json").exists());
}

#[test]
fn completed_cleanup_accepts_crash_subset_of_planned_markers() {
    let fixture = Fixture::new();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    fixture.publish(first, b"first");
    fixture.publish(second, b"second");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 2, aggressive_policy())
        .expect("quarantine");
    let run = fixture.run(run_id);
    let plan_bytes = fs::read(run.join("plan.json")).expect("plan");
    let plan: GcPlan = serde_json::from_slice(&plan_bytes).expect("plan json");
    for candidate in &plan.candidates {
        let item = run.join("items").join(candidate.snapshot_id.to_string());
        fs::remove_file(item.join("payload")).expect("payload");
        fs::remove_file(item.join("manifest.json")).expect("manifest");
        fs::remove_dir(item).expect("item");
    }
    let complete = format!(
        "{{\"version\":1,\"run_id\":\"{run_id}\",\"vault_id\":\"{}\",\"plan_blake3\":\"{}\"}}",
        fixture.vault_id, plan.plan_blake3
    );
    fs::write(run.join("complete.json"), complete).expect("complete");
    fs::set_permissions(run.join("complete.json"), fs::Permissions::from_mode(0o600))
        .expect("private complete");
    fs::remove_file(
        run.join("state")
            .join(format!("{}.json", plan.candidates[0].snapshot_id)),
    )
    .expect("simulate one marker already removed");

    fixture
        .store
        .delete_quarantined_run(run_id)
        .expect("subset cleanup converges");
    assert!(!run.exists());
}

#[test]
fn retained_terminal_makes_absent_run_retries_succeed() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    fixture
        .store
        .delete_quarantined_run(run_id)
        .expect("first delete");
    fixture
        .store
        .delete_quarantined_run(run_id)
        .expect("retained-terminal retry");
    assert!(!fixture.run(run_id).exists());
}

#[test]
fn unknown_absent_run_without_terminal_proof_is_rejected() {
    let fixture = Fixture::new();
    let run_id = Uuid::new_v4();
    assert!(matches!(
        fixture.store.delete_quarantined_run(run_id),
        Err(Error::QuarantineCollision)
    ));
}

#[test]
fn terminal_tombstone_recovers_each_final_cleanup_boundary() {
    for phase in 0_u8..=3 {
        let fixture = Fixture::new();
        let snapshot_id = Uuid::new_v4();
        fixture.publish(snapshot_id, b"payload");
        let run_id = Uuid::new_v4();
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy())
            .expect("quarantine");
        let run = fixture.run(run_id);
        prepare_completed_run(&fixture, run_id, snapshot_id);
        let complete = fs::read(run.join("complete.json")).expect("complete bytes");
        let runs = run.parent().expect("runs");
        let terminal = runs.join(format!(".completed.{run_id}.json"));
        fs::write(&terminal, &complete).expect("terminal");
        fs::set_permissions(&terminal, fs::Permissions::from_mode(0o600))
            .expect("private terminal");
        if phase >= 1 {
            fs::remove_file(run.join("state").join(format!("{snapshot_id}.json")))
                .expect("state marker");
            fs::remove_dir(run.join("state")).expect("state");
            fs::remove_dir(run.join("marker-staging")).expect("marker staging");
            fs::remove_dir(run.join("items")).expect("items");
            fs::remove_file(run.join("plan.json")).expect("plan");
        }
        if phase >= 2 {
            fs::remove_file(run.join("complete.json")).expect("complete removed");
        }
        if phase == 3 {
            fs::remove_dir(&run).expect("run removed");
        }

        fixture
            .store
            .delete_quarantined_run(run_id)
            .expect("terminal recovery");
        assert!(!run.exists());
        assert!(terminal.exists());
        fixture
            .store
            .delete_quarantined_run(run_id)
            .expect("retained terminal retry");
    }
}

#[test]
fn terminal_publication_resumes_exact_attempt_and_rejects_mismatch() {
    for mismatch in [false, true] {
        let fixture = Fixture::new();
        let snapshot_id = Uuid::new_v4();
        fixture.publish(snapshot_id, b"payload");
        let run_id = Uuid::new_v4();
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy())
            .expect("quarantine");
        let run = fixture.run(run_id);
        prepare_completed_run(&fixture, run_id, snapshot_id);
        let complete = fs::read(run.join("complete.json")).expect("complete");
        let attempt = run.parent().expect("runs").join(format!(
            ".completed-attempt.{run_id}.{}.tmp",
            Uuid::new_v4()
        ));
        let attempt_bytes = if mismatch {
            b"opaque".as_slice()
        } else {
            &complete
        };
        fs::write(&attempt, attempt_bytes).expect("terminal attempt");
        fs::set_permissions(&attempt, fs::Permissions::from_mode(0o600)).expect("private attempt");

        let result = fixture.store.delete_quarantined_run(run_id);
        if mismatch {
            assert!(matches!(result, Err(Error::QuarantineCollision)));
            assert_eq!(
                fs::read(&attempt).expect("attempt preserved"),
                attempt_bytes
            );
            assert!(run.join("complete.json").exists());
        } else {
            result.expect("resume exact terminal attempt");
            assert!(!attempt.exists());
            assert!(!run.exists());
        }
    }
}

#[test]
fn malformed_completion_attempt_preflight_preserves_candidate_and_state() {
    let fixture = Fixture::new();
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine");
    let run = fixture.run(run_id);
    let item_payload = run
        .join("items")
        .join(snapshot_id.to_string())
        .join("payload");
    let state = run.join("state").join(format!("{snapshot_id}.json"));
    let payload_before = fs::read(&item_payload).expect("payload before");
    let state_before = fs::read(&state).expect("state before");
    let attempt = run.join(format!(".complete-{}.tmp", Uuid::new_v4()));
    let opaque = b"opaque";
    fs::write(&attempt, opaque).expect("attempt");
    fs::set_permissions(&attempt, fs::Permissions::from_mode(0o600)).expect("private attempt");

    assert!(matches!(
        fixture.store.delete_quarantined_run(run_id),
        Err(Error::QuarantineCollision)
    ));
    assert_eq!(
        fs::read(item_payload).expect("payload preserved"),
        payload_before
    );
    assert_eq!(fs::read(state).expect("state preserved"), state_before);
    assert_eq!(fs::read(attempt).expect("attempt preserved"), opaque);
}

#[test]
fn initial_nested_preflight_prevents_partial_multi_candidate_deletion() {
    for case in 0_u8..5 {
        let fixture = Fixture::new();
        fixture.publish(Uuid::new_v4(), b"first");
        fixture.publish(Uuid::new_v4(), b"second");
        let run_id = Uuid::new_v4();
        fixture
            .store
            .quarantine_retention(run_id, 2, aggressive_policy())
            .expect("quarantine");
        let run = fixture.run(run_id);
        let plan: GcPlan =
            serde_json::from_slice(&fs::read(run.join("plan.json")).expect("plan bytes"))
                .expect("plan");
        let first = plan.candidates[0].snapshot_id;
        let second = plan.candidates[1].snapshot_id;
        let first_item = run.join("items").join(first.to_string());
        let first_manifest = fs::read(first_item.join("manifest.json")).expect("manifest before");
        let first_payload = fs::read(first_item.join("payload")).expect("payload before");
        let first_state_path = run.join("state").join(format!("{first}.json"));
        let first_state = fs::read(&first_state_path).expect("state before");
        let staged = run
            .join("marker-staging")
            .join(format!("{first}.{}.tmp", Uuid::new_v4()));
        fs::write(&staged, &first_state).expect("staged marker");
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))
            .expect("private staged marker");
        match case {
            0 => fs::remove_file(run.join("state").join(format!("{second}.json")))
                .expect("missing second marker"),
            1 => fs::write(run.join("state").join(format!("{second}.json")), b"{}")
                .expect("mismatched second marker"),
            2 => {
                let extra = run.join("state").join(format!("{}.json", Uuid::new_v4()));
                fs::write(&extra, &first_state).expect("extra state");
                fs::set_permissions(extra, fs::Permissions::from_mode(0o600))
                    .expect("private extra state");
            }
            3 => {
                let extra = run.join("items").join(Uuid::new_v4().to_string());
                fs::create_dir(&extra).expect("extra item");
                fs::set_permissions(extra, fs::Permissions::from_mode(0o700))
                    .expect("private extra item");
            }
            4 => {
                let extra = run.join("items").join(second.to_string()).join("unknown");
                fs::write(&extra, b"opaque").expect("unknown topology");
                fs::set_permissions(extra, fs::Permissions::from_mode(0o600))
                    .expect("private unknown");
            }
            _ => unreachable!(),
        }
        let state_names_before = sorted_names(&run.join("state"));
        let staging_names_before = sorted_names(&run.join("marker-staging"));

        assert!(fixture.store.delete_quarantined_run(run_id).is_err());
        assert_eq!(
            fs::read(first_item.join("manifest.json")).expect("manifest preserved"),
            first_manifest
        );
        assert_eq!(
            fs::read(first_item.join("payload")).expect("payload preserved"),
            first_payload
        );
        assert_eq!(
            fs::read(first_state_path).expect("state preserved"),
            first_state
        );
        assert_eq!(fs::read(&staged).expect("staging preserved"), first_state);
        assert_eq!(sorted_names(&run.join("state")), state_names_before);
        assert_eq!(
            sorted_names(&run.join("marker-staging")),
            staging_names_before
        );
    }
}

#[test]
fn completed_nested_preflight_rejects_late_evidence_before_any_cleanup() {
    for malformed_state in [true, false] {
        let fixture = Fixture::new();
        let snapshot_id = Uuid::new_v4();
        fixture.publish(snapshot_id, b"payload");
        let run_id = Uuid::new_v4();
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy())
            .expect("quarantine");
        let run = fixture.run(run_id);
        prepare_completed_run(&fixture, run_id, snapshot_id);
        let state = run.join("state").join(format!("{snapshot_id}.json"));
        if malformed_state {
            fs::write(&state, b"{}").expect("malformed late state");
        } else {
            let unknown = run.join("marker-staging").join("unknown");
            fs::write(&unknown, b"opaque").expect("unknown staging");
            fs::set_permissions(unknown, fs::Permissions::from_mode(0o600))
                .expect("private unknown staging");
        }
        let plan_before = fs::read(run.join("plan.json")).expect("plan before");
        let complete_before = fs::read(run.join("complete.json")).expect("complete before");
        let state_names_before = sorted_names(&run.join("state"));
        let staging_names_before = sorted_names(&run.join("marker-staging"));
        let state_before = fs::read(&state).expect("state before");

        assert!(fixture.store.delete_quarantined_run(run_id).is_err());
        assert_eq!(
            fs::read(run.join("plan.json")).expect("plan preserved"),
            plan_before
        );
        assert_eq!(
            fs::read(run.join("complete.json")).expect("complete preserved"),
            complete_before
        );
        assert_eq!(fs::read(&state).expect("state preserved"), state_before);
        assert_eq!(sorted_names(&run.join("state")), state_names_before);
        assert_eq!(
            sorted_names(&run.join("marker-staging")),
            staging_names_before
        );
        assert!(run.join("items").is_dir());
    }
}

fn sorted_names(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .expect("directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("UTF-8 name")
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn prepare_completed_run(fixture: &Fixture, run_id: Uuid, snapshot_id: Uuid) {
    let run = fixture.run(run_id);
    let plan_bytes = fs::read(run.join("plan.json")).expect("plan");
    let plan: GcPlan = serde_json::from_slice(&plan_bytes).expect("plan json");
    let item = run.join("items").join(snapshot_id.to_string());
    fs::remove_file(item.join("payload")).expect("payload");
    fs::remove_file(item.join("manifest.json")).expect("manifest");
    fs::remove_dir(&item).expect("item");
    let complete = format!(
        "{{\"version\":1,\"run_id\":\"{run_id}\",\"vault_id\":\"{}\",\"plan_blake3\":\"{}\"}}",
        fixture.vault_id, plan.plan_blake3
    );
    fs::write(run.join("complete.json"), complete).expect("complete");
    fs::set_permissions(run.join("complete.json"), fs::Permissions::from_mode(0o600))
        .expect("private complete");
}

#[test]
fn deletion_resolves_staged_markers_for_multiple_planned_candidates() {
    let fixture = Fixture::new();
    let snapshot_ids = [Uuid::new_v4(), Uuid::new_v4()];
    for (index, snapshot_id) in snapshot_ids.into_iter().enumerate() {
        fixture.publish(snapshot_id, format!("payload-{index}").as_bytes());
    }
    let run_id = Uuid::new_v4();
    fixture
        .store
        .quarantine_retention(run_id, 1, aggressive_policy())
        .expect("quarantine both candidates");
    let run = fixture.run(run_id);
    for snapshot_id in snapshot_ids {
        let marker = fs::read(run.join("state").join(format!("{snapshot_id}.json")))
            .expect("detached marker");
        let attempt = run
            .join("marker-staging")
            .join(format!("{snapshot_id}.{}.tmp", Uuid::new_v4()));
        fs::write(&attempt, marker).expect("staged marker attempt");
        fs::set_permissions(&attempt, fs::Permissions::from_mode(0o600))
            .expect("private staged marker");
    }

    fixture
        .store
        .delete_quarantined_run(run_id)
        .expect("delete multi-candidate run");
    assert!(!run.exists());
}

fn plan_logical_bytes(fixture: &Fixture, run_id: Uuid, snapshot_id: Uuid) -> u64 {
    let bytes = fs::read(fixture.run(run_id).join("plan.json")).expect("plan before cleanup");
    let plan: GcPlan = serde_json::from_slice(&bytes).expect("plan");
    plan.candidates
        .iter()
        .find(|candidate| candidate.snapshot_id == snapshot_id)
        .expect("candidate")
        .logical_bytes
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

#[test]
fn retained_terminal_ledgers_do_not_consume_active_cap_but_physical_scan_is_bounded() {
    let fixture = Fixture::new();
    fixture
        .store
        .quarantine_retention(Uuid::new_v4(), 0, RetentionPolicy::default())
        .expect("create roots");
    let runs = fixture.root().join("quarantine/v1/runs");
    for value in 1_u128..=128 {
        let run_id = Uuid::from_u128(value);
        let marker = format!(
            "{{\"version\":1,\"run_id\":\"{run_id}\",\"vault_id\":\"{}\",\"plan_blake3\":\"{}\"}}",
            fixture.vault_id,
            "0".repeat(64)
        );
        let terminal = runs.join(format!(".completed.{run_id}.json"));
        fs::write(&terminal, marker).expect("terminal ledger");
        fs::set_permissions(terminal, fs::Permissions::from_mode(0o600))
            .expect("private terminal ledger");
    }
    fixture.publish(Uuid::new_v4(), b"payload");
    fixture
        .store
        .quarantine_retention(Uuid::new_v4(), 1, aggressive_policy())
        .expect("terminal ledgers excluded from active cap");

    let overflow = Fixture::new();
    overflow
        .store
        .quarantine_retention(Uuid::new_v4(), 0, RetentionPolicy::default())
        .expect("create overflow roots");
    let overflow_runs = overflow.root().join("quarantine/v1/runs");
    for value in 1_u128..=513 {
        let run_id = Uuid::from_u128(value);
        let marker = format!(
            "{{\"version\":1,\"run_id\":\"{run_id}\",\"vault_id\":\"{}\",\"plan_blake3\":\"{}\"}}",
            overflow.vault_id,
            "0".repeat(64)
        );
        let terminal = overflow_runs.join(format!(".completed.{run_id}.json"));
        fs::write(&terminal, marker).expect("overflow terminal ledger");
        fs::set_permissions(terminal, fs::Permissions::from_mode(0o600))
            .expect("private overflow ledger");
    }
    assert!(matches!(
        overflow
            .store
            .quarantine_retention(Uuid::new_v4(), 0, RetentionPolicy::default()),
        Err(Error::TooManyGcRuns)
    ));
}

#[test]
fn five_hundred_eleven_preexisting_entries_reserve_terminal_capacity() {
    let fixture = Fixture::new();
    fixture
        .store
        .quarantine_retention(Uuid::new_v4(), 0, RetentionPolicy::default())
        .expect("create roots");
    for value in 1_u128..=511 {
        write_terminal_bytes(&fixture, Uuid::from_u128(value), &"0".repeat(64));
    }
    let snapshot_id = Uuid::new_v4();
    fixture.publish(snapshot_id, b"payload");
    let run_id = Uuid::new_v4();
    let runs = fixture.root().join("quarantine/v1/runs");
    let before = sorted_names(&runs);

    assert!(matches!(
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy()),
        Err(Error::TooManyGcRuns)
    ));
    assert_eq!(sorted_names(&runs), before);
    assert!(!fixture.run(run_id).exists());
    assert!(fixture
        .root()
        .join("objects")
        .join(snapshot_id.to_string())
        .is_dir());
}

#[test]
fn mismatched_stable_terminal_blocks_active_and_completed_without_mutation() {
    for completed in [false, true] {
        let fixture = Fixture::new();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        fixture.publish(first, b"first");
        fixture.publish(second, b"second");
        let run_id = Uuid::new_v4();
        fixture
            .store
            .quarantine_retention(run_id, 2, aggressive_policy())
            .expect("quarantine");
        let run = fixture.run(run_id);
        if completed {
            for id in [first, second] {
                let item = run.join("items").join(id.to_string());
                fs::remove_file(item.join("payload")).expect("payload");
                fs::remove_file(item.join("manifest.json")).expect("manifest");
                fs::remove_dir(item).expect("item");
            }
            let plan: GcPlan =
                serde_json::from_slice(&fs::read(run.join("plan.json")).expect("plan"))
                    .expect("plan json");
            let complete = format!(
                "{{\"version\":1,\"run_id\":\"{run_id}\",\"vault_id\":\"{}\",\"plan_blake3\":\"{}\"}}",
                fixture.vault_id, plan.plan_blake3
            );
            fs::write(run.join("complete.json"), complete).expect("complete");
            fs::set_permissions(run.join("complete.json"), fs::Permissions::from_mode(0o600))
                .expect("private complete");
        }
        let candidate = if completed { second } else { first };
        let item = run.join("items").join(candidate.to_string());
        let item_before = item.is_dir().then(|| {
            (
                fs::read(item.join("manifest.json")).expect("manifest before"),
                fs::read(item.join("payload")).expect("payload before"),
            )
        });
        let state_path = run.join("state").join(format!("{candidate}.json"));
        let state_before = fs::read(&state_path).expect("state before");
        let complete_before = fs::read(run.join("complete.json")).ok();
        write_terminal_bytes(&fixture, run_id, &"f".repeat(64));

        assert!(matches!(
            fixture.store.delete_quarantined_run(run_id),
            Err(Error::QuarantineCollision)
        ));
        if let Some((manifest, payload)) = item_before {
            assert_eq!(
                fs::read(item.join("manifest.json")).expect("manifest"),
                manifest
            );
            assert_eq!(fs::read(item.join("payload")).expect("payload"), payload);
        }
        assert_eq!(fs::read(state_path).expect("state preserved"), state_before);
        assert_eq!(fs::read(run.join("complete.json")).ok(), complete_before);
    }
}

#[test]
fn fifth_exact_attempt_in_each_family_blocks_without_deletion() {
    for family in 0_u8..3 {
        let fixture = Fixture::new();
        let snapshot_id = Uuid::new_v4();
        fixture.publish(snapshot_id, b"payload");
        let run_id = Uuid::new_v4();
        fixture
            .store
            .quarantine_retention(run_id, 1, aggressive_policy())
            .expect("quarantine");
        let run = fixture.run(run_id);
        let plan: GcPlan =
            serde_json::from_slice(&fs::read(run.join("plan.json")).expect("plan bytes"))
                .expect("plan");
        let completion = format!(
            "{{\"version\":1,\"run_id\":\"{run_id}\",\"vault_id\":\"{}\",\"plan_blake3\":\"{}\"}}",
            fixture.vault_id, plan.plan_blake3
        );
        if family == 2 {
            prepare_completed_run(&fixture, run_id, snapshot_id);
        }
        let marker =
            fs::read(run.join("state").join(format!("{snapshot_id}.json"))).expect("marker");
        let mut attempts = Vec::new();
        for _ in 0..5 {
            let attempt = match family {
                0 => run
                    .join("marker-staging")
                    .join(format!("{snapshot_id}.{}.tmp", Uuid::new_v4())),
                1 => run.join(format!(".complete-{}.tmp", Uuid::new_v4())),
                2 => run.parent().expect("runs").join(format!(
                    ".completed-attempt.{run_id}.{}.tmp",
                    Uuid::new_v4()
                )),
                _ => unreachable!(),
            };
            fs::write(
                &attempt,
                if family == 0 {
                    &marker
                } else {
                    completion.as_bytes()
                },
            )
            .expect("attempt");
            fs::set_permissions(&attempt, fs::Permissions::from_mode(0o600))
                .expect("private attempt");
            attempts.push(attempt);
        }
        let item_payload = run
            .join("items")
            .join(snapshot_id.to_string())
            .join("payload");
        let payload_before = fs::read(&item_payload).ok();
        let complete_before = fs::read(run.join("complete.json")).ok();

        assert!(matches!(
            fixture.store.delete_quarantined_run(run_id),
            Err(Error::QuarantineCollision)
        ));
        for attempt in attempts {
            assert!(attempt.exists());
        }
        assert_eq!(fs::read(&item_payload).ok(), payload_before);
        assert_eq!(fs::read(run.join("complete.json")).ok(), complete_before);
    }
}

#[test]
fn active_and_terminal_duplicate_run_id_fails_closed_in_both_creation_orders() {
    for terminal_first in [false, true] {
        let fixture = Fixture::new();
        fixture
            .store
            .quarantine_retention(Uuid::new_v4(), 0, RetentionPolicy::default())
            .expect("roots");
        let run_id = Uuid::new_v4();
        let run = fixture.run(run_id);
        if terminal_first {
            write_terminal_bytes(&fixture, run_id, &"0".repeat(64));
        }
        fs::create_dir(&run).expect("active duplicate");
        fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).expect("private active");
        if !terminal_first {
            write_terminal_bytes(&fixture, run_id, &"0".repeat(64));
        }
        assert!(matches!(
            fixture
                .store
                .quarantine_retention(Uuid::new_v4(), 0, RetentionPolicy::default()),
            Err(Error::QuarantineCollision)
        ));
    }
}

fn write_terminal_bytes(fixture: &Fixture, run_id: Uuid, digest: &str) {
    let terminal = fixture
        .root()
        .join("quarantine/v1/runs")
        .join(format!(".completed.{run_id}.json"));
    let marker = format!(
        "{{\"version\":1,\"run_id\":\"{run_id}\",\"vault_id\":\"{}\",\"plan_blake3\":\"{digest}\"}}",
        fixture.vault_id
    );
    fs::write(&terminal, marker).expect("terminal");
    fs::set_permissions(terminal, fs::Permissions::from_mode(0o600)).expect("private terminal");
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
