use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io;
use uuid::Uuid;

use super::{
    atomic_rename_noreplace, inspect_object, open_optional_private_dir, private_fs,
    read_optional_private_file, read_private_file, sync_published, write_private_file,
    DurabilityBoundary, Error, EvidenceLocation, RetentionPolicy, RetentionReason,
    SnapshotEvidence, SnapshotStore, MANIFEST_FILE, MAX_MANIFEST_BYTES,
};

const QUARANTINE: &str = "quarantine";
const Q_VERSION: &str = "v1";
const RUNS: &str = "runs";
const WORK: &str = "work";
const PLAN: &str = "plan.json";
const COMPLETE: &str = "complete.json";
const TERMINAL_PREFIX: &str = ".completed.";
const TERMINAL_SUFFIX: &str = ".json";
const TERMINAL_ATTEMPT_PREFIX: &str = ".completed-attempt.";
const ITEMS: &str = "items";
const STATE: &str = "state";
const MARKER_STAGING: &str = "marker-staging";
const MAX_PLAN_BYTES: u64 = 128 * 1024;
const MAX_RUNS: usize = 128;
// Retained completed ledgers are bounded separately from active runs. Ledger
// compaction is intentionally deferred beyond v0.1.
const MAX_PHYSICAL_RUN_ENTRIES: usize = 512;

#[cfg(test)]
static DETACH_FAULT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
#[cfg(test)]
static DELETE_FAULT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
#[cfg(test)]
static DELETE_FAULT_SKIP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static DELETE_LOCK_REPLACE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
static FAULT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[allow(clippy::unnecessary_wraps)]
fn inject_fault(point: u8) -> Result<(), Error> {
    #[cfg(test)]
    if DETACH_FAULT
        .compare_exchange(
            point,
            0,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
    {
        return Err(Error::Io(io::Error::other("injected quarantine fault")));
    }
    let _ = point;
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn inject_delete_fault(point: u8) -> Result<(), Error> {
    #[cfg(test)]
    loop {
        let remaining = DELETE_FAULT_SKIP.load(std::sync::atomic::Ordering::SeqCst);
        if remaining == 0 {
            break;
        }
        if DELETE_FAULT_SKIP
            .compare_exchange(
                remaining,
                remaining - 1,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            return Ok(());
        }
    }
    #[cfg(test)]
    if DELETE_FAULT
        .compare_exchange(
            point,
            0,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
    {
        return Err(Error::Io(io::Error::other("injected deletion fault")));
    }
    let _ = point;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcCandidate {
    pub snapshot_id: Uuid,
    pub manifest_blake3: String,
    pub payload_blake3: String,
    pub payload_bytes: u64,
    pub logical_bytes: u64,
    pub created_at_unix_ms: u64,
    pub lineage_key: String,
    pub lineage_blake3: String,
    pub reasons: Vec<RetentionReason>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcPlan {
    pub version: u32,
    pub run_id: Uuid,
    pub vault_id: Uuid,
    pub policy: RetentionPolicy,
    pub evaluated_at_unix_ms: u64,
    pub candidates: Vec<GcCandidate>,
    pub plan_blake3: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuarantineOutcome {
    Created,
    RecoveredExisting,
    NoCandidates,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantineReport {
    pub outcome: QuarantineOutcome,
    pub run_id: Uuid,
    pub detached: usize,
    pub already_marked: usize,
    pub stale_work_entries: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeletionOutcome {
    Completed,
    Resumed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionReport {
    pub run_id: Uuid,
    pub outcome: DeletionOutcome,
    pub reclaimed_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionMarker {
    version: u32,
    run_id: Uuid,
    vault_id: Uuid,
    plan_blake3: String,
}

impl CompletionMarker {
    fn from_plan(plan: &GcPlan) -> Self {
        Self {
            version: 1,
            run_id: plan.run_id,
            vault_id: plan.vault_id,
            plan_blake3: plan.plan_blake3.clone(),
        }
    }
}

#[derive(Serialize)]
struct UnsignedPlan<'a> {
    version: u32,
    run_id: Uuid,
    vault_id: Uuid,
    policy: RetentionPolicy,
    evaluated_at_unix_ms: u64,
    candidates: &'a [GcCandidate],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DetachedMarker {
    version: u32,
    run_id: Uuid,
    snapshot_id: Uuid,
    plan_blake3: String,
}

impl SnapshotStore {
    /// Creates or resumes one durable quarantine run. This only atomically
    /// detaches immutable objects; it never unlinks or deletes them.
    ///
    /// # Errors
    /// Fails closed on opaque runs, topology mismatches, bounds, or lock loss.
    pub fn quarantine_retention(
        &self,
        run_id: Uuid,
        evaluated_at_unix_ms: u64,
        policy: RetentionPolicy,
    ) -> Result<QuarantineReport, Error> {
        if run_id.is_nil() {
            return Err(Error::InvalidSnapshotId);
        }
        let operation = self.lock_operation()?;
        let result = self.quarantine_locked(run_id, evaluated_at_unix_ms, policy);
        match (result, operation.finish()) {
            (Ok(report), Ok(())) => Ok(report),
            (Ok(report), Err(_)) => Err(Error::QuarantinedButLockLost(report)),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(_)) => Err(Error::OperationFailedAndLockLost(Box::new(error))),
        }
    }

    fn quarantine_locked(
        &self,
        run_id: Uuid,
        evaluated_at_unix_ms: u64,
        policy: RetentionPolicy,
    ) -> Result<QuarantineReport, Error> {
        let (runs, work, stale_work_entries) = self.open_run_roots()?;
        let existing = self.recover_runs(
            &runs,
            Some((run_id, evaluated_at_unix_ms, policy)),
            stale_work_entries,
        )?;
        if let Some(report) = existing {
            return Ok(report);
        }
        let retention = self.plan_retention_locked(evaluated_at_unix_ms, policy)?;
        if retention.candidates.is_empty() {
            return Ok(QuarantineReport {
                outcome: QuarantineOutcome::NoCandidates,
                run_id,
                detached: 0,
                already_marked: 0,
                stale_work_entries,
            });
        }
        if bounded_active_run_count(&runs, self.vault_id)?
            .checked_add(stale_work_entries)
            .ok_or(Error::ArithmeticOverflow)?
            >= MAX_RUNS
        {
            return Err(Error::TooManyGcRuns);
        }
        let candidates = retention
            .candidates
            .iter()
            .map(|candidate| self.bind_candidate(candidate))
            .collect::<Result<Vec<_>, _>>()?;
        let mut plan = GcPlan {
            version: 1,
            run_id,
            vault_id: self.vault_id,
            policy,
            evaluated_at_unix_ms,
            candidates,
            plan_blake3: String::new(),
        };
        plan.plan_blake3 = plan_digest(&plan)?;
        validate_plan(&plan, self.vault_id)?;
        let work_name = format!(".work-{run_id}-{}", Uuid::new_v4());
        let run = private_fs::create_private_dir(&work, &work_name)?;
        let items = private_fs::create_private_dir(&run, ITEMS)?;
        let state = private_fs::create_private_dir(&run, STATE)?;
        let marker_staging = private_fs::create_private_dir(&run, MARKER_STAGING)?;
        write_private_file(&run, PLAN, &serde_json::to_vec(&plan)?)?;
        sync_published(&items, DurabilityBoundary::QuarantineItems)?;
        sync_published(&state, DurabilityBoundary::QuarantineState)?;
        sync_published(&marker_staging, DurabilityBoundary::QuarantineMarkerStaging)?;
        sync_published(&run, DurabilityBoundary::QuarantineRun)?;
        drop((items, state, marker_staging, run));
        atomic_rename_noreplace(&work, &work_name, &runs, &run_id.to_string())?;
        sync_published(&runs, DurabilityBoundary::QuarantineRuns)?;
        sync_published(&work, DurabilityBoundary::QuarantineWork)?;
        let run = private_fs::open_private_dir(&runs, run_id.to_string())?;
        let items = private_fs::open_private_dir(&run, ITEMS)?;
        let state = private_fs::open_private_dir(&run, STATE)?;
        let marker_staging = private_fs::open_private_dir(&run, MARKER_STAGING)?;
        self.process_plan(
            &plan,
            &items,
            &state,
            &marker_staging,
            QuarantineOutcome::Created,
            stale_work_entries,
        )
    }

    fn open_run_roots(&self) -> Result<(cap_std::fs::Dir, cap_std::fs::Dir, usize), Error> {
        let quarantine = private_fs::create_or_open_private_dir(&self.vault, QUARANTINE)?;
        let version = private_fs::create_or_open_private_dir(&quarantine, Q_VERSION)?;
        let runs = private_fs::create_or_open_private_dir(&version, RUNS)?;
        let work = private_fs::create_or_open_private_dir(&version, WORK)?;
        let stale = bounded_entry_count(&work)?;
        Ok((runs, work, stale))
    }

    fn recover_runs(
        &self,
        runs: &cap_std::fs::Dir,
        requested: Option<(Uuid, u64, RetentionPolicy)>,
        stale_work_entries: usize,
    ) -> Result<Option<QuarantineReport>, Error> {
        let mut names = Vec::new();
        let mut seen_run_ids = BTreeSet::new();
        for (entry_count, entry) in runs.entries()?.enumerate() {
            if entry_count == MAX_PHYSICAL_RUN_ENTRIES {
                return Err(Error::TooManyGcRuns);
            }
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or(Error::InvalidGcPlan)?
                .to_owned();
            if let Some(id) = parse_terminal_name(&name) {
                if !seen_run_ids.insert(id) {
                    return Err(Error::QuarantineCollision);
                }
                let bytes = read_private_file(runs, &name, MAX_MANIFEST_BYTES)?;
                validate_completion_standalone(&bytes, id, self.vault_id)?;
                if requested
                    .as_ref()
                    .is_some_and(|(run_id, _, _)| *run_id == id)
                {
                    return Err(Error::QuarantineCollision);
                }
                continue;
            }
            let id = Uuid::parse_str(&name).map_err(|_| Error::InvalidGcPlan)?;
            if id.is_nil() || id.to_string() != name {
                return Err(Error::InvalidGcPlan);
            }
            if !seen_run_ids.insert(id) {
                return Err(Error::QuarantineCollision);
            }
            if names.len() == MAX_RUNS {
                return Err(Error::TooManyGcRuns);
            }
            names.push((id, name));
        }
        names.sort_by_key(|(id, _)| *id);
        let mut loaded = Vec::with_capacity(names.len());
        for (id, name) in names {
            let run = private_fs::open_private_dir(runs, &name)?;
            validate_run_entries(&run)?;
            let bytes = read_private_file(&run, PLAN, MAX_PLAN_BYTES)?;
            let plan: GcPlan = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidGcPlan)?;
            validate_plan(&plan, self.vault_id)?;
            if serde_json::to_vec(&plan)? != bytes || plan.run_id != id {
                return Err(Error::InvalidGcPlan);
            }
            loaded.push((id, run, plan));
        }
        if let Some((requested_id, evaluated, policy)) = requested {
            if let Some((_, _, plan)) = loaded.iter().find(|(id, _, _)| *id == requested_id) {
                if plan.evaluated_at_unix_ms != evaluated || plan.policy != policy {
                    return Err(Error::QuarantineCollision);
                }
            }
        }

        let mut requested_report = None;
        for (id, run, plan) in loaded {
            sync_published(&run, DurabilityBoundary::QuarantineRun)?;
            sync_published(runs, DurabilityBoundary::QuarantineRuns)?;
            let items = private_fs::open_private_dir(&run, ITEMS)?;
            let state = private_fs::open_private_dir(&run, STATE)?;
            let marker_staging = private_fs::open_private_dir(&run, MARKER_STAGING)?;
            let report = self.process_plan(
                &plan,
                &items,
                &state,
                &marker_staging,
                QuarantineOutcome::RecoveredExisting,
                stale_work_entries,
            )?;
            if requested.is_some_and(|(requested_id, _, _)| requested_id == id) {
                requested_report = Some(report);
            }
        }
        Ok(requested_report)
    }

    fn bind_candidate(&self, candidate: &super::RetentionCandidate) -> Result<GcCandidate, Error> {
        let name = candidate.snapshot_id.to_string();
        let source = private_fs::open_private_dir(&self.objects, &name)?;
        let (evidence, logical_bytes) = inspect_object(
            &source,
            EvidenceLocation::Objects,
            candidate.snapshot_id,
            self.vault_id,
            None,
        )?;
        let SnapshotEvidence::Supported { manifest, .. } = evidence? else {
            return Err(Error::InvalidGcPlan);
        };
        let manifest_bytes = read_private_file(&source, MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
        let lineage_key = myvault_core::VaultPath::from_portable(&manifest.path)
            .map_err(|_| Error::InvalidNotePath)?
            .collision_key();
        Ok(GcCandidate {
            snapshot_id: candidate.snapshot_id,
            manifest_blake3: blake3::hash(&manifest_bytes).to_hex().to_string(),
            payload_blake3: manifest.revision.blake3_hex,
            payload_bytes: manifest.revision.byte_len,
            logical_bytes,
            created_at_unix_ms: manifest.created_at_unix_ms,
            lineage_blake3: blake3::hash(lineage_key.as_bytes()).to_hex().to_string(),
            lineage_key,
            reasons: candidate.reasons.clone(),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn process_plan(
        &self,
        plan: &GcPlan,
        items: &cap_std::fs::Dir,
        state: &cap_std::fs::Dir,
        marker_staging: &cap_std::fs::Dir,
        outcome: QuarantineOutcome,
        stale_work_entries: usize,
    ) -> Result<QuarantineReport, Error> {
        validate_plan_children(plan, items, state, marker_staging)?;
        let mut detached = 0;
        let mut already_marked = 0;
        for candidate in &plan.candidates {
            let name = candidate.snapshot_id.to_string();
            let marker_name = format!("{name}.json");
            if let Some(bytes) =
                read_optional_private_file(state, &marker_name, MAX_MANIFEST_BYTES)?
            {
                let marker: DetachedMarker =
                    serde_json::from_slice(&bytes).map_err(|_| Error::QuarantineCollision)?;
                if serde_json::to_vec(&marker)? != bytes
                    || marker.version != 1
                    || marker.run_id != plan.run_id
                    || marker.snapshot_id != candidate.snapshot_id
                    || marker.plan_blake3 != plan.plan_blake3
                {
                    return Err(Error::QuarantineCollision);
                }
                let item = private_fs::open_private_dir(items, &name).map_err(|source| {
                    detached_unknown(plan.run_id, candidate.snapshot_id, source.into())
                })?;
                verify_bound(&item, candidate, self.vault_id).map_err(|source| {
                    detached_unknown(plan.run_id, candidate.snapshot_id, source)
                })?;
                sync_published(&item, DurabilityBoundary::QuarantineItems).map_err(|source| {
                    detached_sync_error(
                        plan.run_id,
                        candidate.snapshot_id,
                        DurabilityBoundary::QuarantineItems,
                        source,
                    )
                })?;
                sync_published(items, DurabilityBoundary::QuarantineItems).map_err(|source| {
                    detached_sync_error(
                        plan.run_id,
                        candidate.snapshot_id,
                        DurabilityBoundary::QuarantineItems,
                        source,
                    )
                })?;
                sync_published(state, DurabilityBoundary::QuarantineState).map_err(|source| {
                    detached_sync_error(
                        plan.run_id,
                        candidate.snapshot_id,
                        DurabilityBoundary::QuarantineState,
                        source,
                    )
                })?;
                sync_published(marker_staging, DurabilityBoundary::QuarantineMarkerStaging)
                    .map_err(|source| {
                        detached_sync_error(
                            plan.run_id,
                            candidate.snapshot_id,
                            DurabilityBoundary::QuarantineMarkerStaging,
                            source,
                        )
                    })?;
                already_marked += 1;
                continue;
            }
            let source = open_optional_private_dir(&self.objects, &name)?;
            let destination = open_optional_private_dir(items, &name)?;
            let target = match (source, destination) {
                (Some(source), None) => {
                    verify_bound(&source, candidate, self.vault_id)?;
                    let identity = private_fs::held_directory_identity(&source)?;
                    drop(source);
                    atomic_rename_noreplace(&self.objects, &name, items, &name)?;
                    inject_fault(1).map_err(|source| {
                        detached_sync_error(
                            plan.run_id,
                            candidate.snapshot_id,
                            DurabilityBoundary::QuarantineItems,
                            source,
                        )
                    })?;
                    sync_published(items, DurabilityBoundary::QuarantineItems).map_err(
                        |source| {
                            detached_sync_error(
                                plan.run_id,
                                candidate.snapshot_id,
                                DurabilityBoundary::QuarantineItems,
                                source,
                            )
                        },
                    )?;
                    inject_fault(2).map_err(|source| {
                        detached_sync_error(
                            plan.run_id,
                            candidate.snapshot_id,
                            DurabilityBoundary::SourceObjects,
                            source,
                        )
                    })?;
                    sync_published(&self.objects, DurabilityBoundary::SourceObjects).map_err(
                        |source| {
                            detached_sync_error(
                                plan.run_id,
                                candidate.snapshot_id,
                                DurabilityBoundary::SourceObjects,
                                source,
                            )
                        },
                    )?;
                    inject_fault(3).map_err(|source| {
                        detached_unknown(plan.run_id, candidate.snapshot_id, source)
                    })?;
                    let target = private_fs::open_private_dir(items, &name).map_err(|source| {
                        detached_unknown(plan.run_id, candidate.snapshot_id, source.into())
                    })?;
                    if private_fs::held_directory_identity(&target).map_err(|source| {
                        detached_unknown(plan.run_id, candidate.snapshot_id, source.into())
                    })? != identity
                    {
                        return Err(detached_unknown(
                            plan.run_id,
                            candidate.snapshot_id,
                            Error::QuarantineCollision,
                        ));
                    }
                    detached += 1;
                    target
                }
                (None, Some(target)) => {
                    verify_bound(&target, candidate, self.vault_id).map_err(|source| {
                        detached_unknown(plan.run_id, candidate.snapshot_id, source)
                    })?;
                    sync_published(items, DurabilityBoundary::QuarantineItems).map_err(
                        |source| {
                            detached_sync_error(
                                plan.run_id,
                                candidate.snapshot_id,
                                DurabilityBoundary::QuarantineItems,
                                source,
                            )
                        },
                    )?;
                    sync_published(&self.objects, DurabilityBoundary::SourceObjects).map_err(
                        |source| {
                            detached_sync_error(
                                plan.run_id,
                                candidate.snapshot_id,
                                DurabilityBoundary::SourceObjects,
                                source,
                            )
                        },
                    )?;
                    target
                }
                _ => return Err(Error::QuarantineCollision),
            };
            verify_bound(&target, candidate, self.vault_id)
                .map_err(|source| detached_unknown(plan.run_id, candidate.snapshot_id, source))?;
            publish_marker(marker_staging, state, plan, candidate).map_err(|source| {
                preserve_detached_error(plan.run_id, candidate.snapshot_id, source)
            })?;
        }
        Ok(QuarantineReport {
            outcome,
            run_id: plan.run_id,
            detached,
            already_marked,
            stale_work_entries,
        })
    }
}

fn detached_sync_error(
    run_id: Uuid,
    snapshot_id: Uuid,
    boundary: DurabilityBoundary,
    source: Error,
) -> Error {
    Error::DetachedButNotSynced {
        run_id,
        snapshot_id,
        boundary,
        source: Box::new(source),
    }
}

fn detached_unknown(run_id: Uuid, snapshot_id: Uuid, source: Error) -> Error {
    Error::DetachedOutcomeUnknown {
        run_id,
        snapshot_id,
        source: Box::new(source),
    }
}

fn preserve_detached_error(run_id: Uuid, snapshot_id: Uuid, source: Error) -> Error {
    match source {
        Error::DetachedButNotSynced { .. } | Error::DetachedOutcomeUnknown { .. } => source,
        other => detached_unknown(run_id, snapshot_id, other),
    }
}

fn validate_plan_children(
    plan: &GcPlan,
    items: &cap_std::fs::Dir,
    state: &cap_std::fs::Dir,
    marker_staging: &cap_std::fs::Dir,
) -> Result<(), Error> {
    let item_names = plan
        .candidates
        .iter()
        .map(|candidate| candidate.snapshot_id.to_string())
        .collect::<BTreeSet<_>>();
    let marker_names = item_names
        .iter()
        .map(|name| format!("{name}.json"))
        .collect::<BTreeSet<_>>();
    for entry in items.entries()? {
        let name = entry?
            .file_name()
            .to_str()
            .ok_or(Error::QuarantineCollision)?
            .to_owned();
        if !item_names.contains(&name) {
            return Err(Error::QuarantineCollision);
        }
    }
    for entry in state.entries()? {
        let name = entry?
            .file_name()
            .to_str()
            .ok_or(Error::QuarantineCollision)?
            .to_owned();
        if !marker_names.contains(&name) {
            return Err(Error::QuarantineCollision);
        }
    }
    let mut staged_count = 0;
    let mut per_candidate = std::collections::BTreeMap::<Uuid, usize>::new();
    for entry in marker_staging.entries()? {
        staged_count += 1;
        let name = entry?
            .file_name()
            .to_str()
            .ok_or(Error::QuarantineCollision)?
            .to_owned();
        let Some((snapshot_id, _attempt)) = parse_marker_attempt(&name) else {
            return Err(Error::QuarantineCollision);
        };
        if !item_names.contains(&snapshot_id.to_string()) {
            return Err(Error::QuarantineCollision);
        }
        read_private_file(marker_staging, &name, MAX_MANIFEST_BYTES)
            .map_err(|_| Error::QuarantineCollision)?;
        let count = per_candidate.entry(snapshot_id).or_default();
        *count += 1;
        if *count > 4 || staged_count > plan.candidates.len().saturating_mul(4) {
            return Err(Error::QuarantineCollision);
        }
    }
    Ok(())
}

fn verify_bound(
    directory: &cap_std::fs::Dir,
    candidate: &GcCandidate,
    vault_id: Uuid,
) -> Result<(), Error> {
    let (evidence, logical) = inspect_object(
        directory,
        EvidenceLocation::Objects,
        candidate.snapshot_id,
        vault_id,
        None,
    )?;
    let SnapshotEvidence::Supported { manifest, .. } = evidence? else {
        return Err(Error::QuarantineCollision);
    };
    let bytes = read_private_file(directory, MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
    if logical != candidate.logical_bytes
        || blake3::hash(&bytes).to_hex().as_str() != candidate.manifest_blake3
        || manifest.revision.blake3_hex != candidate.payload_blake3
        || manifest.revision.byte_len != candidate.payload_bytes
        || manifest.created_at_unix_ms != candidate.created_at_unix_ms
    {
        return Err(Error::QuarantineCollision);
    }
    let lineage_key = myvault_core::VaultPath::from_portable(&manifest.path)
        .map_err(|_| Error::QuarantineCollision)?
        .collision_key();
    if lineage_key != candidate.lineage_key
        || blake3::hash(lineage_key.as_bytes()).to_hex().as_str() != candidate.lineage_blake3
    {
        return Err(Error::QuarantineCollision);
    }
    Ok(())
}

fn plan_digest(plan: &GcPlan) -> Result<String, Error> {
    let unsigned = UnsignedPlan {
        version: plan.version,
        run_id: plan.run_id,
        vault_id: plan.vault_id,
        policy: plan.policy,
        evaluated_at_unix_ms: plan.evaluated_at_unix_ms,
        candidates: &plan.candidates,
    };
    Ok(blake3::hash(&serde_json::to_vec(&unsigned)?)
        .to_hex()
        .to_string())
}

fn validate_plan(plan: &GcPlan, vault_id: Uuid) -> Result<(), Error> {
    if plan.version != 1
        || plan.run_id.is_nil()
        || plan.vault_id != vault_id
        || plan.candidates.is_empty()
        || plan.candidates.len() > super::MAX_RETENTION_CANDIDATES
        || plan.plan_blake3 != plan_digest(plan)?
    {
        return Err(Error::InvalidGcPlan);
    }
    let mut ids = BTreeSet::new();
    if plan.candidates.iter().any(|candidate| {
        let reasons = candidate.reasons.iter().copied().collect::<BTreeSet<_>>();
        !ids.insert(candidate.snapshot_id)
            || candidate.snapshot_id.is_nil()
            || !is_digest(&candidate.manifest_blake3)
            || !is_digest(&candidate.payload_blake3)
            || candidate.payload_bytes > super::MAX_PAYLOAD_BYTES
            || candidate.logical_bytes < candidate.payload_bytes
            || candidate.logical_bytes - candidate.payload_bytes > super::MAX_MANIFEST_BYTES
            || candidate.reasons.is_empty()
            || reasons.len() != candidate.reasons.len()
            || reasons.into_iter().collect::<Vec<_>>() != candidate.reasons
            || candidate.lineage_blake3
                != blake3::hash(candidate.lineage_key.as_bytes())
                    .to_hex()
                    .to_string()
    }) {
        return Err(Error::InvalidGcPlan);
    }
    let bytes = serde_json::to_vec(plan)?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err(Error::GcPlanTooLarge);
    }
    Ok(())
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn publish_marker(
    marker_staging: &cap_std::fs::Dir,
    state: &cap_std::fs::Dir,
    plan: &GcPlan,
    candidate: &GcCandidate,
) -> Result<(), Error> {
    let marker = DetachedMarker {
        version: 1,
        run_id: plan.run_id,
        snapshot_id: candidate.snapshot_id,
        plan_blake3: plan.plan_blake3.clone(),
    };
    let name = format!("{}.json", candidate.snapshot_id);
    let bytes = serde_json::to_vec(&marker)?;
    if let Some(actual) = read_optional_private_file(state, &name, MAX_MANIFEST_BYTES)? {
        if actual != bytes {
            return Err(Error::QuarantineCollision);
        }
        return sync_marker_parents(marker_staging, state, plan, candidate);
    }
    let mut attempts = Vec::new();
    for entry in marker_staging.entries()? {
        let entry = entry?;
        let attempt = entry
            .file_name()
            .to_str()
            .ok_or(Error::QuarantineCollision)?
            .to_owned();
        if parse_marker_attempt(&attempt).is_some_and(|(id, _)| id == candidate.snapshot_id) {
            attempts.push(attempt);
        }
    }
    attempts.sort();
    let mut temporary = None;
    for attempt in &attempts {
        if read_private_file(marker_staging, attempt, MAX_MANIFEST_BYTES)? == bytes {
            temporary = Some(attempt.clone());
            break;
        }
    }
    if temporary.is_none() {
        if attempts.len() >= 4 {
            return Err(Error::QuarantineCollision);
        }
        let attempt = format!("{}.{}.tmp", candidate.snapshot_id, Uuid::new_v4());
        write_private_file(marker_staging, &attempt, &bytes)?;
        temporary = Some(attempt);
    }
    let temporary = temporary.expect("selected or created marker attempt");
    match atomic_rename_noreplace(marker_staging, &temporary, state, &name) {
        Ok(()) => {
            inject_fault(4).map_err(|source| {
                detached_sync_error(
                    plan.run_id,
                    candidate.snapshot_id,
                    DurabilityBoundary::QuarantineState,
                    source,
                )
            })?;
            sync_published(state, DurabilityBoundary::QuarantineState).map_err(|source| {
                detached_sync_error(
                    plan.run_id,
                    candidate.snapshot_id,
                    DurabilityBoundary::QuarantineState,
                    source,
                )
            })?;
            inject_fault(5).map_err(|source| {
                detached_sync_error(
                    plan.run_id,
                    candidate.snapshot_id,
                    DurabilityBoundary::QuarantineMarkerStaging,
                    source,
                )
            })?;
            sync_marker_staging(marker_staging, plan, candidate)
        }
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            let actual = read_private_file(state, &name, MAX_MANIFEST_BYTES)?;
            if actual != bytes {
                return Err(Error::QuarantineCollision);
            }
            sync_marker_parents(marker_staging, state, plan, candidate)
        }
        Err(error) => Err(error),
    }
}

fn sync_marker_parents(
    marker_staging: &cap_std::fs::Dir,
    state: &cap_std::fs::Dir,
    plan: &GcPlan,
    candidate: &GcCandidate,
) -> Result<(), Error> {
    sync_published(state, DurabilityBoundary::QuarantineState).map_err(|source| {
        detached_sync_error(
            plan.run_id,
            candidate.snapshot_id,
            DurabilityBoundary::QuarantineState,
            source,
        )
    })?;
    sync_marker_staging(marker_staging, plan, candidate)
}

fn sync_marker_staging(
    marker_staging: &cap_std::fs::Dir,
    plan: &GcPlan,
    candidate: &GcCandidate,
) -> Result<(), Error> {
    sync_published(marker_staging, DurabilityBoundary::QuarantineMarkerStaging).map_err(|source| {
        detached_sync_error(
            plan.run_id,
            candidate.snapshot_id,
            DurabilityBoundary::QuarantineMarkerStaging,
            source,
        )
    })
}

fn parse_marker_attempt(name: &str) -> Option<(Uuid, Uuid)> {
    let stem = name.strip_suffix(".tmp")?;
    let (snapshot_text, attempt_text) = stem.split_once('.')?;
    let snapshot = Uuid::parse_str(snapshot_text).ok()?;
    let attempt = Uuid::parse_str(attempt_text).ok()?;
    if snapshot.is_nil()
        || attempt.is_nil()
        || snapshot.to_string() != snapshot_text
        || attempt.to_string() != attempt_text
    {
        return None;
    }
    Some((snapshot, attempt))
}

fn validate_run_entries(run: &cap_std::fs::Dir) -> Result<(), Error> {
    let mut names = Vec::new();
    for entry in run.entries()? {
        if names.len() == 4 {
            return Err(Error::InvalidGcPlan);
        }
        names.push(entry?.file_name());
    }
    names.sort();
    if names != [ITEMS, MARKER_STAGING, PLAN, STATE] {
        return Err(Error::InvalidGcPlan);
    }
    Ok(())
}

fn bounded_entry_count(directory: &cap_std::fs::Dir) -> Result<usize, Error> {
    let mut count = 0;
    for entry in directory.entries()? {
        entry?;
        if count == MAX_RUNS {
            return Err(Error::TooManyGcRuns);
        }
        count += 1;
    }
    Ok(count)
}

fn bounded_active_run_count(runs: &cap_std::fs::Dir, vault_id: Uuid) -> Result<usize, Error> {
    let mut active = 0_usize;
    let mut last_physical = None;
    let mut seen_run_ids = BTreeSet::new();
    for (physical, entry) in runs.entries()?.enumerate() {
        if physical == MAX_PHYSICAL_RUN_ENTRIES {
            return Err(Error::TooManyGcRuns);
        }
        last_physical = Some(physical);
        let name = entry?
            .file_name()
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or(Error::InvalidGcPlan)?;
        if let Some(run_id) = parse_terminal_name(&name) {
            if !seen_run_ids.insert(run_id) {
                return Err(Error::QuarantineCollision);
            }
            let bytes = read_private_file(runs, &name, MAX_MANIFEST_BYTES)?;
            validate_completion_standalone(&bytes, run_id, vault_id)?;
            continue;
        }
        let run_id = Uuid::parse_str(&name).map_err(|_| Error::InvalidGcPlan)?;
        if run_id.is_nil() || run_id.to_string() != name {
            return Err(Error::InvalidGcPlan);
        }
        if !seen_run_ids.insert(run_id) {
            return Err(Error::QuarantineCollision);
        }
        active = active.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
    }
    // Creating an active run consumes one entry and its eventual terminal
    // publication needs one additional crash-safe slot. Therefore creation is
    // allowed only with at most 510 pre-existing physical entries.
    if last_physical.is_some_and(|index| index >= MAX_PHYSICAL_RUN_ENTRIES - 2) {
        return Err(Error::TooManyGcRuns);
    }
    Ok(active)
}

impl SnapshotStore {
    /// Physically removes only marker-authorized quarantined evidence, then
    /// publishes completion and cleans recognized run metadata. Vault objects
    /// are never opened or consulted by this operation. A bounded canonical
    /// terminal ledger is retained so absent-run retries remain provable;
    /// ledger compaction is deferred beyond v0.1.
    ///
    /// # Errors
    /// Fails closed on missing/mismatched markers, unexpected topology, identity
    /// changes, or durability failures after a removal.
    pub fn delete_quarantined_run(&self, run_id: Uuid) -> Result<DeletionReport, Error> {
        if run_id.is_nil() {
            return Err(Error::InvalidSnapshotId);
        }
        let operation = self.lock_operation()?;
        let result = self.delete_quarantined_locked(run_id);
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        if DELETE_LOCK_REPLACE.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.vault.remove_file(super::OPERATION_LOCK_FILE)?;
            let replacement = self.vault.create(super::OPERATION_LOCK_FILE)?;
            private_fs::set_private_file_permissions(&replacement)?;
        }
        match (result, operation.finish()) {
            (Ok(report), Ok(())) => Ok(report),
            (Ok(report), Err(_)) => Err(Error::DeletedButLockLost(report)),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(_)) => Err(Error::OperationFailedAndLockLost(Box::new(error))),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn delete_quarantined_locked(&self, run_id: Uuid) -> Result<DeletionReport, Error> {
        let (runs, _work, _) = self.open_run_roots()?;
        let terminal = read_terminal(&runs, run_id, self.vault_id)?;
        let Some(run) = open_optional_private_dir(&runs, &run_id.to_string())? else {
            let complete = terminal.ok_or(Error::QuarantineCollision)?;
            preflight_terminal_family(&runs, &complete, true)?;
            finalize_terminal(&runs, &complete)?;
            return Ok(DeletionReport {
                run_id,
                outcome: DeletionOutcome::Resumed,
                reclaimed_bytes: 0,
            });
        };
        if run.entries()?.next().transpose()?.is_none() {
            let complete = terminal.ok_or(Error::QuarantineCollision)?;
            preflight_terminal_family(&runs, &complete, true)?;
            let identity = private_fs::held_directory_identity(&run)?;
            inject_delete_fault(1)?;
            private_fs::remove_empty_private_dir_if_identity(
                &runs,
                run_id.to_string(),
                &run,
                &identity,
            )?;
            inject_delete_fault(2).map_err(|source| Error::RemovedButNotSynced {
                run_id,
                snapshot_id: None,
                boundary: DurabilityBoundary::DeletionRuns,
                source: Box::new(source),
            })?;
            sync_remove(&runs, run_id, None, DurabilityBoundary::DeletionRuns)?;
            inject_delete_fault(3).map_err(|source| Error::RemovedAndSyncedButInterrupted {
                run_id,
                snapshot_id: None,
                boundary: DurabilityBoundary::DeletionRuns,
                source: Box::new(source),
            })?;
            finalize_terminal(&runs, &complete)?;
            return Ok(DeletionReport {
                run_id,
                outcome: DeletionOutcome::Resumed,
                reclaimed_bytes: 0,
            });
        }
        if let Some(bytes) = read_optional_private_file(&run, COMPLETE, MAX_MANIFEST_BYTES)? {
            let completion = validate_completion_standalone(&bytes, run_id, self.vault_id)?;
            preflight_terminal_family(&runs, &completion, true)?;
            sync_published(&run, DurabilityBoundary::DeletionRun)?;
            cleanup_completed_run_without_plan(&runs, &run, &completion)?;
            return Ok(DeletionReport {
                run_id,
                outcome: DeletionOutcome::Resumed,
                reclaimed_bytes: 0,
            });
        }
        let plan_bytes = read_private_file(&run, PLAN, MAX_PLAN_BYTES)?;
        let plan: GcPlan = serde_json::from_slice(&plan_bytes).map_err(|_| Error::InvalidGcPlan)?;
        validate_plan(&plan, self.vault_id)?;
        if plan.run_id != run_id || serde_json::to_vec(&plan)? != plan_bytes {
            return Err(Error::InvalidGcPlan);
        }
        let expected_completion = CompletionMarker::from_plan(&plan);
        preflight_run_root(&run, &expected_completion, false)?;
        preflight_terminal_family(&runs, &expected_completion, false)?;
        let items = private_fs::open_private_dir(&run, ITEMS)?;
        let state = private_fs::open_private_dir(&run, STATE)?;
        let marker_staging = private_fs::open_private_dir(&run, MARKER_STAGING)?;
        preflight_initial_nested(&items, &state, &marker_staging, &plan)?;
        let mut reclaimed_bytes = 0_u64;
        for candidate in &plan.candidates {
            validate_detached_marker(&state, &plan, candidate)?;
            resolve_marker_attempts_for_delete(&marker_staging, &state, &plan, candidate)?;
            loop {
                let reclaimed = delete_candidate_item(&items, &plan, candidate)?;
                reclaimed_bytes = reclaimed_bytes
                    .checked_add(reclaimed)
                    .ok_or(Error::ArithmeticOverflow)?;
                if reclaimed != 0
                    || open_optional_private_dir(&items, &candidate.snapshot_id.to_string())?
                        .is_none()
                {
                    break;
                }
            }
        }
        if items.entries()?.next().transpose()?.is_some()
            || marker_staging.entries()?.next().transpose()?.is_some()
        {
            return Err(Error::QuarantineCollision);
        }
        validate_state_exact(&state, &plan)?;
        publish_completion(&run, &plan)?;
        cleanup_completed_run(&runs, &run, &plan)?;
        Ok(DeletionReport {
            run_id,
            outcome: if reclaimed_bytes == 0 {
                DeletionOutcome::Resumed
            } else {
                DeletionOutcome::Completed
            },
            reclaimed_bytes,
        })
    }
}

fn validate_detached_marker(
    state: &cap_std::fs::Dir,
    plan: &GcPlan,
    candidate: &GcCandidate,
) -> Result<Vec<u8>, Error> {
    let name = format!("{}.json", candidate.snapshot_id);
    let bytes = read_private_file(state, &name, MAX_MANIFEST_BYTES)?;
    let marker: DetachedMarker = serde_json::from_slice(&bytes)?;
    if serde_json::to_vec(&marker)? != bytes
        || marker.version != 1
        || marker.run_id != plan.run_id
        || marker.snapshot_id != candidate.snapshot_id
        || marker.plan_blake3 != plan.plan_blake3
    {
        return Err(Error::QuarantineCollision);
    }
    Ok(bytes)
}

fn validate_state_exact(state: &cap_std::fs::Dir, plan: &GcPlan) -> Result<(), Error> {
    let expected = plan
        .candidates
        .iter()
        .map(|candidate| format!("{}.json", candidate.snapshot_id))
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for entry in state.entries()? {
        if observed.len() == plan.candidates.len() {
            return Err(Error::QuarantineCollision);
        }
        let name = entry?
            .file_name()
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or(Error::QuarantineCollision)?;
        observed.insert(name);
    }
    if observed != expected {
        return Err(Error::QuarantineCollision);
    }
    Ok(())
}

fn validate_state_subset(state: &cap_std::fs::Dir, plan: &GcPlan) -> Result<Vec<Uuid>, Error> {
    let expected = plan
        .candidates
        .iter()
        .map(|candidate| {
            (
                format!("{}.json", candidate.snapshot_id),
                candidate.snapshot_id,
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut observed = Vec::new();
    for entry in state.entries()? {
        if observed.len() == plan.candidates.len() {
            return Err(Error::QuarantineCollision);
        }
        let name = entry?
            .file_name()
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or(Error::QuarantineCollision)?;
        observed.push(*expected.get(&name).ok_or(Error::QuarantineCollision)?);
    }
    Ok(observed)
}

fn preflight_initial_nested(
    items: &cap_std::fs::Dir,
    state: &cap_std::fs::Dir,
    marker_staging: &cap_std::fs::Dir,
    plan: &GcPlan,
) -> Result<(), Error> {
    validate_state_exact(state, plan)?;
    let mut expected_markers = std::collections::BTreeMap::new();
    for candidate in &plan.candidates {
        expected_markers.insert(
            candidate.snapshot_id,
            validate_detached_marker(state, plan, candidate)?,
        );
    }
    let staging_limit = plan
        .candidates
        .len()
        .checked_mul(4)
        .ok_or(Error::ArithmeticOverflow)?;
    let mut staging_per_candidate = std::collections::BTreeMap::<Uuid, usize>::new();
    for (staging_total, entry) in marker_staging.entries()?.enumerate() {
        if staging_total == staging_limit {
            return Err(Error::QuarantineCollision);
        }
        let name = entry?
            .file_name()
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or(Error::QuarantineCollision)?;
        let (snapshot_id, _) = parse_marker_attempt(&name).ok_or(Error::QuarantineCollision)?;
        let expected = expected_markers
            .get(&snapshot_id)
            .ok_or(Error::QuarantineCollision)?;
        let family_count = staging_per_candidate.entry(snapshot_id).or_default();
        *family_count += 1;
        if *family_count > 4 {
            return Err(Error::QuarantineCollision);
        }
        if read_private_file(marker_staging, &name, MAX_MANIFEST_BYTES)? != *expected {
            return Err(Error::QuarantineCollision);
        }
    }
    let planned = plan
        .candidates
        .iter()
        .map(|candidate| candidate.snapshot_id)
        .collect::<BTreeSet<_>>();
    for (item_count, entry) in items.entries()?.enumerate() {
        if item_count == plan.candidates.len() {
            return Err(Error::QuarantineCollision);
        }
        let name = entry?
            .file_name()
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or(Error::QuarantineCollision)?;
        let snapshot_id = Uuid::parse_str(&name).map_err(|_| Error::QuarantineCollision)?;
        if snapshot_id.is_nil()
            || snapshot_id.to_string() != name
            || !planned.contains(&snapshot_id)
        {
            return Err(Error::QuarantineCollision);
        }
    }
    for candidate in &plan.candidates {
        preflight_candidate_item(items, plan, candidate)?;
    }
    Ok(())
}

fn preflight_candidate_item(
    items: &cap_std::fs::Dir,
    plan: &GcPlan,
    candidate: &GcCandidate,
) -> Result<(), Error> {
    let Some(item) = open_optional_private_dir(items, &candidate.snapshot_id.to_string())? else {
        return Ok(());
    };
    let names = bounded_item_names(&item)?;
    if names == [MANIFEST_FILE, super::PAYLOAD_FILE] {
        verify_bound(&item, candidate, plan.vault_id)?;
    } else if names == [MANIFEST_FILE] {
        validate_manifest_only(&item, candidate)?;
    } else if !names.is_empty() {
        return Err(Error::QuarantineCollision);
    }
    Ok(())
}

fn bounded_item_names(item: &cap_std::fs::Dir) -> Result<Vec<std::ffi::OsString>, Error> {
    let mut names = Vec::new();
    for entry in item.entries()? {
        if names.len() == 2 {
            return Err(Error::QuarantineCollision);
        }
        names.push(entry?.file_name());
    }
    names.sort();
    Ok(names)
}

fn bounded_utf8_names(directory: &cap_std::fs::Dir, maximum: usize) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    for entry in directory.entries()? {
        if names.len() == maximum {
            return Err(Error::QuarantineCollision);
        }
        names.push(
            entry?
                .file_name()
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or(Error::QuarantineCollision)?,
        );
    }
    Ok(names)
}

fn resolve_marker_attempts_for_delete(
    staging: &cap_std::fs::Dir,
    state: &cap_std::fs::Dir,
    plan: &GcPlan,
    candidate: &GcCandidate,
) -> Result<(), Error> {
    let expected = validate_detached_marker(state, plan, candidate)?;
    let staging_limit = plan
        .candidates
        .len()
        .checked_mul(4)
        .ok_or(Error::ArithmeticOverflow)?;
    let mut own_attempts = 0_usize;
    for name in bounded_utf8_names(staging, staging_limit)? {
        let (attempt_snapshot_id, _) =
            parse_marker_attempt(&name).ok_or(Error::QuarantineCollision)?;
        if attempt_snapshot_id != candidate.snapshot_id {
            if plan
                .candidates
                .iter()
                .any(|planned| planned.snapshot_id == attempt_snapshot_id)
            {
                continue;
            }
            return Err(Error::QuarantineCollision);
        }
        own_attempts += 1;
        if own_attempts > 4 {
            return Err(Error::QuarantineCollision);
        }
        if read_private_file(staging, &name, MAX_MANIFEST_BYTES)? != expected {
            return Err(Error::QuarantineCollision);
        }
        remove_file_synced(
            staging,
            &name,
            plan.run_id,
            Some(candidate.snapshot_id),
            DurabilityBoundary::QuarantineMarkerStaging,
        )?;
    }
    Ok(())
}

fn delete_candidate_item(
    items: &cap_std::fs::Dir,
    plan: &GcPlan,
    candidate: &GcCandidate,
) -> Result<u64, Error> {
    let name = candidate.snapshot_id.to_string();
    let Some(item) = open_optional_private_dir(items, &name)? else {
        return Ok(0);
    };
    let names = bounded_item_names(&item)?;
    if names == [MANIFEST_FILE, super::PAYLOAD_FILE] {
        verify_bound(&item, candidate, plan.vault_id)?;
        remove_file_synced(
            &item,
            super::PAYLOAD_FILE,
            plan.run_id,
            Some(candidate.snapshot_id),
            DurabilityBoundary::DeletionItem,
        )?;
        return Ok(0);
    }
    if names == [MANIFEST_FILE] {
        validate_manifest_only(&item, candidate)?;
        remove_file_synced(
            &item,
            MANIFEST_FILE,
            plan.run_id,
            Some(candidate.snapshot_id),
            DurabilityBoundary::DeletionItem,
        )?;
        return Ok(0);
    }
    if names.is_empty() {
        let identity = private_fs::held_directory_identity(&item)?;
        inject_delete_fault(1)?;
        private_fs::remove_empty_private_dir_if_identity(items, &name, &item, &identity)?;
        inject_delete_fault(2).map_err(|source| Error::RemovedButNotSynced {
            run_id: plan.run_id,
            snapshot_id: Some(candidate.snapshot_id),
            boundary: DurabilityBoundary::DeletionItems,
            source: Box::new(source),
        })?;
        sync_remove(
            items,
            plan.run_id,
            Some(candidate.snapshot_id),
            DurabilityBoundary::DeletionItems,
        )?;
        inject_delete_fault(3).map_err(|source| Error::RemovedAndSyncedButInterrupted {
            run_id: plan.run_id,
            snapshot_id: Some(candidate.snapshot_id),
            boundary: DurabilityBoundary::DeletionItems,
            source: Box::new(source),
        })?;
        return Ok(candidate.logical_bytes);
    }
    Err(Error::QuarantineCollision)
}

fn validate_manifest_only(item: &cap_std::fs::Dir, candidate: &GcCandidate) -> Result<(), Error> {
    let bytes = read_private_file(item, MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
    let manifest: super::SnapshotManifest = serde_json::from_slice(&bytes)?;
    manifest.validate()?;
    let lineage = myvault_core::VaultPath::from_portable(&manifest.path)
        .map_err(|_| Error::QuarantineCollision)?
        .collision_key();
    let logical = u64::try_from(bytes.len())
        .map_err(|_| Error::ArithmeticOverflow)?
        .checked_add(manifest.revision.byte_len)
        .ok_or(Error::ArithmeticOverflow)?;
    if serde_json::to_vec(&manifest)? != bytes
        || blake3::hash(&bytes).to_hex().as_str() != candidate.manifest_blake3
        || manifest.snapshot_id != candidate.snapshot_id
        || manifest.revision.blake3_hex != candidate.payload_blake3
        || manifest.revision.byte_len != candidate.payload_bytes
        || logical != candidate.logical_bytes
        || lineage != candidate.lineage_key
        || blake3::hash(lineage.as_bytes()).to_hex().as_str() != candidate.lineage_blake3
    {
        return Err(Error::QuarantineCollision);
    }
    Ok(())
}

fn publish_completion(run: &cap_std::fs::Dir, plan: &GcPlan) -> Result<(), Error> {
    let marker = CompletionMarker::from_plan(plan);
    let bytes = serde_json::to_vec(&marker)?;
    let mut attempts = Vec::new();
    for (entry_count, entry) in run.entries()?.enumerate() {
        if entry_count == 9 {
            return Err(Error::QuarantineCollision);
        }
        let name = entry?
            .file_name()
            .to_str()
            .ok_or(Error::QuarantineCollision)?
            .to_owned();
        if parse_completion_attempt(&name).is_some() {
            if attempts.len() == 4 {
                return Err(Error::QuarantineCollision);
            }
            attempts.push(name);
        }
    }
    let mut temporary = None;
    for attempt in &attempts {
        if read_private_file(run, attempt, MAX_MANIFEST_BYTES)? == bytes {
            temporary = Some(attempt.clone());
            break;
        }
    }
    let temporary = if let Some(attempt) = temporary {
        attempt
    } else {
        if attempts.len() >= 4 {
            return Err(Error::QuarantineCollision);
        }
        let attempt = format!(".complete-{}.tmp", Uuid::new_v4());
        write_private_file(run, &attempt, &bytes)?;
        attempt
    };
    match atomic_rename_noreplace(run, &temporary, run, COMPLETE) {
        Ok(()) => sync_published(run, DurabilityBoundary::DeletionRun),
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_completion(&read_private_file(run, COMPLETE, MAX_MANIFEST_BYTES)?, plan)?;
            sync_published(run, DurabilityBoundary::DeletionRun)
        }
        Err(error) => Err(error),
    }
}

fn validate_completion(bytes: &[u8], plan: &GcPlan) -> Result<(), Error> {
    let marker = validate_completion_standalone(bytes, plan.run_id, plan.vault_id)?;
    if marker.plan_blake3 != plan.plan_blake3 {
        return Err(Error::QuarantineCollision);
    }
    Ok(())
}

fn validate_completion_standalone(
    bytes: &[u8],
    run_id: Uuid,
    vault_id: Uuid,
) -> Result<CompletionMarker, Error> {
    let marker: CompletionMarker = serde_json::from_slice(bytes)?;
    if serde_json::to_vec(&marker)? != bytes
        || marker.version != 1
        || marker.run_id != run_id
        || marker.vault_id != vault_id
        || !is_digest(&marker.plan_blake3)
    {
        return Err(Error::QuarantineCollision);
    }
    Ok(marker)
}

fn preflight_run_root(
    run: &cap_std::fs::Dir,
    complete: &CompletionMarker,
    completion_published: bool,
) -> Result<(), Error> {
    let expected = serde_json::to_vec(complete)?;
    let mut controls = BTreeSet::new();
    let mut completion_attempts = 0_usize;
    for (entry_count, entry) in run.entries()?.enumerate() {
        if entry_count == 9 {
            return Err(Error::QuarantineCollision);
        }
        let name = entry?
            .file_name()
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or(Error::QuarantineCollision)?;
        match name.as_str() {
            PLAN | ITEMS | STATE | MARKER_STAGING => {
                controls.insert(name);
            }
            COMPLETE if completion_published => {
                if read_private_file(run, COMPLETE, MAX_MANIFEST_BYTES)? != expected {
                    return Err(Error::QuarantineCollision);
                }
                controls.insert(name);
            }
            _ if parse_completion_attempt(&name).is_some() => {
                completion_attempts += 1;
                if completion_attempts > 4 {
                    return Err(Error::QuarantineCollision);
                }
                if read_private_file(run, &name, MAX_MANIFEST_BYTES)? != expected {
                    return Err(Error::QuarantineCollision);
                }
            }
            _ => return Err(Error::QuarantineCollision),
        }
    }
    if completion_published {
        if !controls.contains(COMPLETE) {
            return Err(Error::QuarantineCollision);
        }
    } else if controls
        != BTreeSet::from([
            PLAN.to_owned(),
            ITEMS.to_owned(),
            STATE.to_owned(),
            MARKER_STAGING.to_owned(),
        ])
    {
        return Err(Error::QuarantineCollision);
    }
    Ok(())
}

fn terminal_name(run_id: Uuid) -> String {
    format!("{TERMINAL_PREFIX}{run_id}{TERMINAL_SUFFIX}")
}

fn parse_terminal_name(name: &str) -> Option<Uuid> {
    let text = name
        .strip_prefix(TERMINAL_PREFIX)?
        .strip_suffix(TERMINAL_SUFFIX)?;
    let run_id = Uuid::parse_str(text).ok()?;
    (!run_id.is_nil() && run_id.to_string() == text).then_some(run_id)
}

fn terminal_attempt_name(run_id: Uuid, attempt_id: Uuid) -> String {
    format!("{TERMINAL_ATTEMPT_PREFIX}{run_id}.{attempt_id}.tmp")
}

fn parse_terminal_attempt(name: &str) -> Option<(Uuid, Uuid)> {
    let text = name
        .strip_prefix(TERMINAL_ATTEMPT_PREFIX)?
        .strip_suffix(".tmp")?;
    let (run_text, attempt_text) = text.split_once('.')?;
    let run_id = Uuid::parse_str(run_text).ok()?;
    let attempt_id = Uuid::parse_str(attempt_text).ok()?;
    (!run_id.is_nil()
        && !attempt_id.is_nil()
        && run_id.to_string() == run_text
        && attempt_id.to_string() == attempt_text)
        .then_some((run_id, attempt_id))
}

fn preflight_terminal_family(
    runs: &cap_std::fs::Dir,
    expected: &CompletionMarker,
    allow_stable: bool,
) -> Result<(), Error> {
    let expected_bytes = serde_json::to_vec(expected)?;
    let stable_name = terminal_name(expected.run_id);
    let attempt_prefix = format!("{TERMINAL_ATTEMPT_PREFIX}{}.", expected.run_id);
    let mut attempts = 0_usize;
    for (physical, entry) in runs.entries()?.enumerate() {
        if physical == MAX_PHYSICAL_RUN_ENTRIES {
            return Err(Error::TooManyGcRuns);
        }
        let name = entry?
            .file_name()
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or(Error::QuarantineCollision)?;
        if name == stable_name {
            if !allow_stable
                || read_private_file(runs, &name, MAX_MANIFEST_BYTES)? != expected_bytes
            {
                return Err(Error::QuarantineCollision);
            }
        } else if name.starts_with(&attempt_prefix) {
            if !allow_stable {
                return Err(Error::QuarantineCollision);
            }
            if parse_terminal_attempt(&name).is_none_or(|(id, _)| id != expected.run_id)
                || read_private_file(runs, &name, MAX_MANIFEST_BYTES)? != expected_bytes
            {
                return Err(Error::QuarantineCollision);
            }
            attempts += 1;
            if attempts > 4 {
                return Err(Error::QuarantineCollision);
            }
        }
    }
    Ok(())
}

fn read_terminal(
    runs: &cap_std::fs::Dir,
    run_id: Uuid,
    vault_id: Uuid,
) -> Result<Option<CompletionMarker>, Error> {
    let Some(bytes) = read_optional_private_file(runs, &terminal_name(run_id), MAX_MANIFEST_BYTES)?
    else {
        return Ok(None);
    };
    validate_completion_standalone(&bytes, run_id, vault_id).map(Some)
}

fn publish_terminal(runs: &cap_std::fs::Dir, complete: &CompletionMarker) -> Result<(), Error> {
    let name = terminal_name(complete.run_id);
    let bytes = serde_json::to_vec(complete)?;
    if let Some(existing) = read_optional_private_file(runs, &name, MAX_MANIFEST_BYTES)? {
        if existing != bytes {
            return Err(Error::QuarantineCollision);
        }
        return sync_published(runs, DurabilityBoundary::DeletionRuns);
    }
    let mut attempts = Vec::new();
    let attempt_prefix = format!("{TERMINAL_ATTEMPT_PREFIX}{}.", complete.run_id);
    let mut last_physical = None;
    for (physical, entry) in runs.entries()?.enumerate() {
        if physical == MAX_PHYSICAL_RUN_ENTRIES {
            return Err(Error::TooManyGcRuns);
        }
        last_physical = Some(physical);
        let entry_name = entry?
            .file_name()
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or(Error::QuarantineCollision)?;
        if entry_name.starts_with(&attempt_prefix)
            && parse_terminal_attempt(&entry_name).is_none_or(|(id, _)| id != complete.run_id)
        {
            return Err(Error::QuarantineCollision);
        }
        if parse_terminal_attempt(&entry_name).is_some_and(|(id, _)| id == complete.run_id) {
            if read_private_file(runs, &entry_name, MAX_MANIFEST_BYTES)? != bytes {
                return Err(Error::QuarantineCollision);
            }
            attempts.push(entry_name);
        }
    }
    if attempts.len() > 4 {
        return Err(Error::QuarantineCollision);
    }
    let temporary = if let Some(existing) = attempts.first() {
        existing.clone()
    } else {
        if last_physical == Some(MAX_PHYSICAL_RUN_ENTRIES - 1) {
            return Err(Error::TooManyGcRuns);
        }
        let attempt = terminal_attempt_name(complete.run_id, Uuid::new_v4());
        write_private_file(runs, &attempt, &bytes)?;
        attempt
    };
    match atomic_rename_noreplace(runs, &temporary, runs, &name) {
        Ok(()) => {}
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            if read_private_file(runs, &name, MAX_MANIFEST_BYTES)? != bytes {
                return Err(Error::QuarantineCollision);
            }
        }
        Err(error) => return Err(error),
    }
    sync_published(runs, DurabilityBoundary::DeletionRuns)
}

fn finalize_terminal(runs: &cap_std::fs::Dir, complete: &CompletionMarker) -> Result<(), Error> {
    let name = terminal_name(complete.run_id);
    let expected = serde_json::to_vec(complete)?;
    if read_private_file(runs, &name, MAX_MANIFEST_BYTES)? != expected {
        return Err(Error::QuarantineCollision);
    }
    let entries = bounded_utf8_names(runs, MAX_PHYSICAL_RUN_ENTRIES)?;
    let mut own_attempts = Vec::new();
    for attempt in entries {
        if parse_terminal_attempt(&attempt).is_some_and(|(id, _)| id == complete.run_id) {
            if own_attempts.len() == 4 {
                return Err(Error::QuarantineCollision);
            }
            if read_private_file(runs, &attempt, MAX_MANIFEST_BYTES)? != expected {
                return Err(Error::QuarantineCollision);
            }
            own_attempts.push(attempt);
        }
    }
    for attempt in own_attempts {
        remove_file_synced(
            runs,
            &attempt,
            complete.run_id,
            None,
            DurabilityBoundary::DeletionRuns,
        )?;
    }
    sync_published(runs, DurabilityBoundary::DeletionRuns)
}

fn cleanup_completed_run(
    runs: &cap_std::fs::Dir,
    run: &cap_std::fs::Dir,
    plan: &GcPlan,
) -> Result<(), Error> {
    let bytes = read_private_file(run, COMPLETE, MAX_MANIFEST_BYTES)?;
    let complete = validate_completion_standalone(&bytes, plan.run_id, plan.vault_id)?;
    cleanup_completed_run_without_plan(runs, run, &complete)
}

fn preflight_completed_nested(
    run: &cap_std::fs::Dir,
    complete: &CompletionMarker,
) -> Result<Option<GcPlan>, Error> {
    preflight_run_root(run, complete, true)?;
    let plan = if let Some(plan_bytes) = read_optional_private_file(run, PLAN, MAX_PLAN_BYTES)? {
        let plan: GcPlan = serde_json::from_slice(&plan_bytes)?;
        validate_plan(&plan, complete.vault_id)?;
        if serde_json::to_vec(&plan)? != plan_bytes
            || plan.run_id != complete.run_id
            || plan.plan_blake3 != complete.plan_blake3
        {
            return Err(Error::QuarantineCollision);
        }
        Some(plan)
    } else {
        None
    };
    if let Some(items) = open_optional_private_dir(run, ITEMS)? {
        if items.entries()?.next().transpose()?.is_some() {
            return Err(Error::QuarantineCollision);
        }
    }
    if let Some(state) = open_optional_private_dir(run, STATE)? {
        let plan = plan.as_ref().ok_or(Error::QuarantineCollision)?;
        for snapshot_id in validate_state_subset(&state, plan)? {
            let candidate = plan
                .candidates
                .iter()
                .find(|candidate| candidate.snapshot_id == snapshot_id)
                .ok_or(Error::QuarantineCollision)?;
            validate_detached_marker(&state, plan, candidate)?;
        }
    }
    if let Some(staging) = open_optional_private_dir(run, MARKER_STAGING)? {
        if staging.entries()?.next().transpose()?.is_some() {
            return Err(Error::QuarantineCollision);
        }
    }
    Ok(plan)
}

#[allow(clippy::too_many_lines)]
fn cleanup_completed_run_without_plan(
    runs: &cap_std::fs::Dir,
    run: &cap_std::fs::Dir,
    complete: &CompletionMarker,
) -> Result<(), Error> {
    preflight_terminal_family(runs, complete, true)?;
    let plan = preflight_completed_nested(run, complete)?;
    sync_published(run, DurabilityBoundary::DeletionRun)?;
    if let Some(state) = open_optional_private_dir(run, STATE)? {
        let plan = plan.as_ref().ok_or(Error::QuarantineCollision)?;
        let observed = validate_state_subset(&state, plan)?;
        for snapshot_id in observed {
            let candidate = plan
                .candidates
                .iter()
                .find(|candidate| candidate.snapshot_id == snapshot_id)
                .ok_or(Error::QuarantineCollision)?;
            validate_detached_marker(&state, plan, candidate)?;
            let name = format!("{}.json", candidate.snapshot_id);
            remove_file_synced(
                &state,
                &name,
                complete.run_id,
                Some(candidate.snapshot_id),
                DurabilityBoundary::QuarantineState,
            )?;
        }
        remove_empty_child_synced(run, STATE, complete.run_id, DurabilityBoundary::DeletionRun)?;
    }
    if let Some(staging) = open_optional_private_dir(run, MARKER_STAGING)? {
        if staging.entries()?.next().transpose()?.is_some() {
            return Err(Error::QuarantineCollision);
        }
        remove_empty_child_synced(
            run,
            MARKER_STAGING,
            complete.run_id,
            DurabilityBoundary::DeletionRun,
        )?;
    }
    if open_optional_private_dir(run, ITEMS)?.is_some() {
        remove_empty_child_synced(run, ITEMS, complete.run_id, DurabilityBoundary::DeletionRun)?;
    }
    if plan.is_some() {
        remove_file_synced(
            run,
            PLAN,
            complete.run_id,
            None,
            DurabilityBoundary::DeletionRun,
        )?;
    }
    remove_completion_attempts(run, complete)?;
    preflight_run_root(run, complete, true)?;
    publish_terminal(runs, complete)?;
    remove_file_synced(
        run,
        COMPLETE,
        complete.run_id,
        None,
        DurabilityBoundary::DeletionRun,
    )?;
    let identity = private_fs::held_directory_identity(run)?;
    inject_delete_fault(1)?;
    private_fs::remove_empty_private_dir_if_identity(
        runs,
        complete.run_id.to_string(),
        run,
        &identity,
    )?;
    inject_delete_fault(2).map_err(|source| Error::RemovedButNotSynced {
        run_id: complete.run_id,
        snapshot_id: None,
        boundary: DurabilityBoundary::DeletionRuns,
        source: Box::new(source),
    })?;
    sync_remove(
        runs,
        complete.run_id,
        None,
        DurabilityBoundary::DeletionRuns,
    )?;
    inject_delete_fault(3).map_err(|source| Error::RemovedAndSyncedButInterrupted {
        run_id: complete.run_id,
        snapshot_id: None,
        boundary: DurabilityBoundary::DeletionRuns,
        source: Box::new(source),
    })?;
    finalize_terminal(runs, complete)
}

fn parse_completion_attempt(name: &str) -> Option<Uuid> {
    let id = name.strip_prefix(".complete-")?.strip_suffix(".tmp")?;
    let parsed = Uuid::parse_str(id).ok()?;
    (!parsed.is_nil() && parsed.to_string() == id).then_some(parsed)
}

fn remove_completion_attempts(
    run: &cap_std::fs::Dir,
    complete: &CompletionMarker,
) -> Result<(), Error> {
    let expected = serde_json::to_vec(complete)?;
    let names = bounded_utf8_names(run, 9)?;
    for name in &names {
        if name == COMPLETE {
            continue;
        }
        if parse_completion_attempt(name).is_none()
            || read_private_file(run, name, MAX_MANIFEST_BYTES)? != expected
        {
            return Err(Error::QuarantineCollision);
        }
    }
    if names
        .iter()
        .filter(|name| name.as_str() != COMPLETE)
        .count()
        > 4
    {
        return Err(Error::QuarantineCollision);
    }
    for name in names.into_iter().filter(|name| name != COMPLETE) {
        remove_file_synced(
            run,
            &name,
            complete.run_id,
            None,
            DurabilityBoundary::DeletionRun,
        )?;
    }
    Ok(())
}

fn remove_file_synced(
    parent: &cap_std::fs::Dir,
    name: &str,
    run_id: Uuid,
    snapshot_id: Option<Uuid>,
    boundary: DurabilityBoundary,
) -> Result<(), Error> {
    inject_delete_fault(1)?;
    let file = private_fs::open_private_file(parent, name, 1)?;
    let identity = private_fs::held_private_file_identity(&file)?;
    private_fs::remove_private_file_if_identity(parent, name, &file, &identity)?;
    inject_delete_fault(2).map_err(|source| Error::RemovedButNotSynced {
        run_id,
        snapshot_id,
        boundary,
        source: Box::new(source),
    })?;
    sync_remove(parent, run_id, snapshot_id, boundary)?;
    inject_delete_fault(3).map_err(|source| Error::RemovedAndSyncedButInterrupted {
        run_id,
        snapshot_id,
        boundary,
        source: Box::new(source),
    })
}

fn remove_empty_child_synced(
    parent: &cap_std::fs::Dir,
    name: &str,
    run_id: Uuid,
    boundary: DurabilityBoundary,
) -> Result<(), Error> {
    inject_delete_fault(1)?;
    let child = private_fs::open_private_dir(parent, name)?;
    let identity = private_fs::held_directory_identity(&child)?;
    private_fs::remove_empty_private_dir_if_identity(parent, name, &child, &identity)?;
    inject_delete_fault(2).map_err(|source| Error::RemovedButNotSynced {
        run_id,
        snapshot_id: None,
        boundary,
        source: Box::new(source),
    })?;
    sync_remove(parent, run_id, None, boundary)?;
    inject_delete_fault(3).map_err(|source| Error::RemovedAndSyncedButInterrupted {
        run_id,
        snapshot_id: None,
        boundary,
        source: Box::new(source),
    })
}

fn sync_remove(
    directory: &cap_std::fs::Dir,
    run_id: Uuid,
    snapshot_id: Option<Uuid>,
    boundary: DurabilityBoundary,
) -> Result<(), Error> {
    sync_published(directory, boundary).map_err(|source| Error::RemovedButNotSynced {
        run_id,
        snapshot_id,
        boundary,
        source: Box::new(source),
    })
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod fault_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn lock_fault_tests() -> std::sync::MutexGuard<'static, ()> {
        let guard = FAULT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        DETACH_FAULT.store(0, std::sync::atomic::Ordering::SeqCst);
        DELETE_FAULT.store(0, std::sync::atomic::Ordering::SeqCst);
        DELETE_FAULT_SKIP.store(0, std::sync::atomic::Ordering::SeqCst);
        DELETE_LOCK_REPLACE.store(false, std::sync::atomic::Ordering::SeqCst);
        guard
    }

    #[test]
    fn completed_deletion_report_is_preserved_when_lock_identity_is_lost() {
        let _fault_guard = lock_fault_tests();
        let temporary = tempfile::tempdir().expect("temporary");
        let base = temporary.path().canonicalize().expect("canonical");
        let app = base.join("app");
        let vault = base.join("vault");
        fs::create_dir(&app).expect("app");
        fs::create_dir(&vault).expect("vault");
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app");
        let vault_id = Uuid::new_v4();
        let store = SnapshotStore::open(&app, &vault, vault_id).expect("store");
        let snapshot_id = Uuid::new_v4();
        let payload = b"lock loss payload";
        let manifest = super::super::SnapshotManifest::new(
            snapshot_id,
            vault_id,
            "lock.md",
            0,
            super::super::SnapshotRevision::from_bytes(payload),
        )
        .expect("manifest");
        store.publish(&manifest, payload).expect("publish");
        let run_id = Uuid::new_v4();
        store
            .quarantine_retention(
                run_id,
                1,
                RetentionPolicy {
                    max_age_ms: 0,
                    max_per_lineage: usize::MAX,
                    max_logical_bytes: u64::MAX,
                },
            )
            .expect("quarantine");

        DELETE_LOCK_REPLACE.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = store
            .delete_quarantined_run(run_id)
            .expect_err("lock replacement");
        assert!(matches!(
            error,
            Error::DeletedButLockLost(DeletionReport {
                run_id: id,
                outcome: DeletionOutcome::Completed,
                ..
            }) if id == run_id
        ));
        let run = app
            .join("recovery-snapshots/v1/vaults")
            .join(vault_id.to_string())
            .join("quarantine/v1/runs")
            .join(run_id.to_string());
        assert!(!run.exists());
    }

    #[test]
    fn terminal_publication_uses_reserved_slot_and_exact_stable_retry_creates_no_attempt() {
        let _fault_guard = lock_fault_tests();
        let temporary = tempfile::tempdir().expect("temporary");
        let base = temporary.path().canonicalize().expect("canonical");
        let app = base.join("app");
        let vault = base.join("vault");
        fs::create_dir(&app).expect("app");
        fs::create_dir(&vault).expect("vault");
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app");
        let vault_id = Uuid::new_v4();
        let store = SnapshotStore::open(&app, &vault, vault_id).expect("store");
        store
            .quarantine_retention(Uuid::new_v4(), 0, RetentionPolicy::default())
            .expect("create roots");
        let runs = app
            .join("recovery-snapshots/v1/vaults")
            .join(vault_id.to_string())
            .join("quarantine/v1/runs");
        for value in 1_u128..=510 {
            let ledger_id = Uuid::from_u128(value);
            let marker = CompletionMarker {
                version: 1,
                run_id: ledger_id,
                vault_id,
                plan_blake3: "0".repeat(64),
            };
            let path = runs.join(terminal_name(ledger_id));
            fs::write(&path, serde_json::to_vec(&marker).expect("marker"))
                .expect("terminal ledger");
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private ledger");
        }
        let snapshot_id = Uuid::new_v4();
        let payload = b"reserved terminal slot";
        let manifest = super::super::SnapshotManifest::new(
            snapshot_id,
            vault_id,
            "capacity.md",
            0,
            super::super::SnapshotRevision::from_bytes(payload),
        )
        .expect("manifest");
        store.publish(&manifest, payload).expect("publish");
        let run_id = Uuid::new_v4();
        store
            .quarantine_retention(
                run_id,
                1,
                RetentionPolicy {
                    max_age_ms: 0,
                    max_per_lineage: usize::MAX,
                    max_logical_bytes: u64::MAX,
                },
            )
            .expect("active run fits reserved capacity");
        let run = runs.join(run_id.to_string());
        let plan: GcPlan =
            serde_json::from_slice(&fs::read(run.join(PLAN)).expect("plan bytes")).expect("plan");
        let completion =
            serde_json::to_vec(&CompletionMarker::from_plan(&plan)).expect("completion");
        let item = run.join(ITEMS).join(snapshot_id.to_string());
        fs::remove_file(item.join(super::super::PAYLOAD_FILE)).expect("payload");
        fs::remove_file(item.join(MANIFEST_FILE)).expect("manifest");
        fs::remove_dir(item).expect("item");
        fs::remove_file(run.join(STATE).join(format!("{snapshot_id}.json"))).expect("state marker");
        fs::remove_dir(run.join(STATE)).expect("state");
        fs::remove_dir(run.join(MARKER_STAGING)).expect("marker staging");
        fs::remove_dir(run.join(ITEMS)).expect("items");
        fs::remove_file(run.join(PLAN)).expect("plan");
        fs::write(run.join(COMPLETE), &completion).expect("complete");
        fs::set_permissions(run.join(COMPLETE), fs::Permissions::from_mode(0o600))
            .expect("private complete");

        DELETE_FAULT.store(1, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            store.delete_quarantined_run(run_id),
            Err(Error::Io(_))
        ));
        assert_eq!(fs::read_dir(&runs).expect("runs").count(), 512);
        assert!(runs.join(terminal_name(run_id)).is_file());
        assert_eq!(
            fs::read_dir(&runs)
                .expect("runs")
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.file_name().to_str().is_some_and(|name| {
                        parse_terminal_attempt(name).is_some_and(|(id, _)| id == run_id)
                    })
                })
                .count(),
            0
        );
        store
            .delete_quarantined_run(run_id)
            .expect("exact stable terminal retry");
        assert_eq!(fs::read_dir(&runs).expect("runs").count(), 511);
        assert!(runs.join(terminal_name(run_id)).is_file());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_deletion_removal_boundary_is_typed_and_recoverable() {
        let _fault_guard = lock_fault_tests();
        for removal_index in 0_usize..12 {
            for point in 1_u8..=3 {
                let temporary = tempfile::tempdir().expect("temporary");
                let base = temporary.path().canonicalize().expect("canonical");
                let app = base.join("app");
                let vault = base.join("vault");
                fs::create_dir(&app).expect("app");
                fs::create_dir(&vault).expect("vault");
                fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app");
                let vault_id = Uuid::new_v4();
                let store = SnapshotStore::open(&app, &vault, vault_id).expect("store");
                let snapshot_id = Uuid::new_v4();
                let payload = b"all boundaries";
                let manifest = super::super::SnapshotManifest::new(
                    snapshot_id,
                    vault_id,
                    "boundaries.md",
                    0,
                    super::super::SnapshotRevision::from_bytes(payload),
                )
                .expect("manifest");
                store.publish(&manifest, payload).expect("publish");
                let run_id = Uuid::new_v4();
                store
                    .quarantine_retention(
                        run_id,
                        1,
                        RetentionPolicy {
                            max_age_ms: 0,
                            max_per_lineage: usize::MAX,
                            max_logical_bytes: u64::MAX,
                        },
                    )
                    .expect("quarantine");
                let run = app
                    .join("recovery-snapshots/v1/vaults")
                    .join(vault_id.to_string())
                    .join("quarantine/v1/runs")
                    .join(run_id.to_string());
                let plan_bytes = fs::read(run.join(PLAN)).expect("plan");
                let plan: GcPlan = serde_json::from_slice(&plan_bytes).expect("plan json");
                let completion = serde_json::to_vec(&CompletionMarker::from_plan(&plan))
                    .expect("completion bytes");
                let runs = run.parent().expect("runs");
                let target_removal = if removal_index == 11 {
                    let item = run.join(ITEMS).join(snapshot_id.to_string());
                    fs::remove_file(item.join(super::super::PAYLOAD_FILE)).expect("payload");
                    fs::remove_file(item.join(MANIFEST_FILE)).expect("manifest");
                    fs::remove_dir(item).expect("item");
                    fs::remove_file(run.join(STATE).join(format!("{snapshot_id}.json")))
                        .expect("state marker");
                    fs::remove_dir(run.join(STATE)).expect("state");
                    fs::remove_dir(run.join(MARKER_STAGING)).expect("marker staging");
                    fs::remove_dir(run.join(ITEMS)).expect("items");
                    fs::remove_file(run.join(PLAN)).expect("plan");
                    fs::write(run.join(COMPLETE), &completion).expect("complete");
                    fs::set_permissions(run.join(COMPLETE), fs::Permissions::from_mode(0o600))
                        .expect("private complete");
                    fs::write(runs.join(terminal_name(run_id)), &completion).expect("terminal");
                    fs::set_permissions(
                        runs.join(terminal_name(run_id)),
                        fs::Permissions::from_mode(0o600),
                    )
                    .expect("private terminal");
                    for _ in 0..2 {
                        let attempt = runs.join(terminal_attempt_name(run_id, Uuid::new_v4()));
                        fs::write(&attempt, &completion).expect("terminal attempt");
                        fs::set_permissions(&attempt, fs::Permissions::from_mode(0o600))
                            .expect("private terminal attempt");
                    }
                    2
                } else {
                    for _ in 0..2 {
                        let attempt = run.join(format!(".complete-{}.tmp", Uuid::new_v4()));
                        fs::write(&attempt, &completion).expect("completion attempt");
                        fs::set_permissions(&attempt, fs::Permissions::from_mode(0o600))
                            .expect("private attempt");
                    }
                    removal_index
                };

                DELETE_FAULT_SKIP.store(
                    target_removal * 3 + usize::from(point - 1),
                    std::sync::atomic::Ordering::SeqCst,
                );
                DELETE_FAULT.store(point, std::sync::atomic::Ordering::SeqCst);
                let error = store
                    .delete_quarantined_run(run_id)
                    .expect_err("injected removal boundary");
                let (boundary, expected_snapshot) = match removal_index {
                    0 | 1 => (DurabilityBoundary::DeletionItem, Some(snapshot_id)),
                    2 => (DurabilityBoundary::DeletionItems, Some(snapshot_id)),
                    3 => (DurabilityBoundary::QuarantineState, Some(snapshot_id)),
                    4..=9 => (DurabilityBoundary::DeletionRun, None),
                    10 | 11 => (DurabilityBoundary::DeletionRuns, None),
                    _ => unreachable!(),
                };
                let typed = match (&error, point) {
                    (Error::Io(_), 1) => true,
                    (
                        Error::RemovedButNotSynced {
                            snapshot_id: actual,
                            boundary: actual_boundary,
                            ..
                        },
                        2,
                    )
                    | (
                        Error::RemovedAndSyncedButInterrupted {
                            snapshot_id: actual,
                            boundary: actual_boundary,
                            ..
                        },
                        3,
                    ) => *actual == expected_snapshot && *actual_boundary == boundary,
                    _ => false,
                };
                assert!(
                    typed,
                    "unexpected error at removal {removal_index}, point {point}: {error}"
                );
                let retry = store.delete_quarantined_run(run_id);
                retry.expect("retry converges");
            }
        }
    }

    #[test]
    fn deletion_remove_faults_are_typed_and_retries_converge() {
        let _fault_guard = lock_fault_tests();
        for item_state in 0_u8..=2 {
            for point in 1_u8..=3 {
                let temporary = tempfile::tempdir().expect("temporary");
                let base = temporary.path().canonicalize().expect("canonical");
                let app = base.join("app");
                let vault = base.join("vault");
                fs::create_dir(&app).expect("app");
                fs::create_dir(&vault).expect("vault");
                fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app");
                let vault_id = Uuid::new_v4();
                let store = SnapshotStore::open(&app, &vault, vault_id).expect("store");
                let snapshot_id = Uuid::new_v4();
                let payload = b"fault payload";
                let manifest = super::super::SnapshotManifest::new(
                    snapshot_id,
                    vault_id,
                    "fault.md",
                    0,
                    super::super::SnapshotRevision::from_bytes(payload),
                )
                .expect("manifest");
                store.publish(&manifest, payload).expect("publish");
                let run_id = Uuid::new_v4();
                store
                    .quarantine_retention(
                        run_id,
                        1,
                        RetentionPolicy {
                            max_age_ms: 0,
                            max_per_lineage: usize::MAX,
                            max_logical_bytes: u64::MAX,
                        },
                    )
                    .expect("quarantine");
                store.publish(&manifest, payload).expect("republish object");
                let root = app
                    .join("recovery-snapshots/v1/vaults")
                    .join(vault_id.to_string());
                let object = root.join("objects").join(snapshot_id.to_string());
                let object_before =
                    fs::read(object.join(super::super::PAYLOAD_FILE)).expect("object before");
                let item = root
                    .join("quarantine/v1/runs")
                    .join(run_id.to_string())
                    .join(ITEMS)
                    .join(snapshot_id.to_string());
                if item_state >= 1 {
                    fs::remove_file(item.join(super::super::PAYLOAD_FILE)).expect("remove payload");
                }
                if item_state == 2 {
                    fs::remove_file(item.join(MANIFEST_FILE)).expect("remove manifest");
                }

                DELETE_FAULT.store(point, std::sync::atomic::Ordering::SeqCst);
                let error = store
                    .delete_quarantined_run(run_id)
                    .expect_err("injected deletion fault");
                if point == 1 {
                    assert!(matches!(error, Error::Io(_)));
                } else {
                    let expected_boundary = if item_state == 2 {
                        DurabilityBoundary::DeletionItems
                    } else {
                        DurabilityBoundary::DeletionItem
                    };
                    let typed = if point == 2 {
                        matches!(
                            &error,
                            Error::RemovedButNotSynced {
                                snapshot_id: Some(id),
                                boundary,
                                ..
                            } if *id == snapshot_id && *boundary == expected_boundary
                        )
                    } else {
                        matches!(
                            &error,
                            Error::RemovedAndSyncedButInterrupted {
                                snapshot_id: Some(id),
                                boundary,
                                ..
                            } if *id == snapshot_id && *boundary == expected_boundary
                        )
                    };
                    assert!(
                        typed,
                        "unexpected deletion fault for state {item_state}, point {point}, snapshot {snapshot_id}, boundary {expected_boundary:?}: {error}"
                    );
                }
                store
                    .delete_quarantined_run(run_id)
                    .expect("deletion retry converges");
                assert_eq!(
                    fs::read(object.join(super::super::PAYLOAD_FILE)).expect("object after"),
                    object_before
                );
            }
        }
    }

    #[test]
    fn post_rename_faults_preserve_detached_fact_and_retry_destination_only() {
        let _fault_guard = lock_fault_tests();
        for point in 1_u8..=5 {
            let temporary = tempfile::tempdir().expect("temporary");
            let base = temporary.path().canonicalize().expect("canonical");
            let app = base.join("app");
            let vault = base.join("vault");
            fs::create_dir(&app).expect("app");
            fs::create_dir(&vault).expect("vault");
            fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app");
            let vault_id = Uuid::new_v4();
            let store = SnapshotStore::open(&app, &vault, vault_id).expect("store");
            let snapshot_id = Uuid::new_v4();
            let payload = b"fault payload";
            let manifest = super::super::SnapshotManifest::new(
                snapshot_id,
                vault_id,
                "fault.md",
                0,
                super::super::SnapshotRevision::from_bytes(payload),
            )
            .expect("manifest");
            store.publish(&manifest, payload).expect("publish");
            let run_id = Uuid::new_v4();
            DETACH_FAULT.store(point, std::sync::atomic::Ordering::SeqCst);
            let error = store
                .quarantine_retention(
                    run_id,
                    1,
                    RetentionPolicy {
                        max_age_ms: 0,
                        max_per_lineage: usize::MAX,
                        max_logical_bytes: u64::MAX,
                    },
                )
                .expect_err("injected fault");
            match (point, error) {
                (
                    1,
                    Error::DetachedButNotSynced {
                        boundary: DurabilityBoundary::QuarantineItems,
                        ..
                    },
                )
                | (
                    2,
                    Error::DetachedButNotSynced {
                        boundary: DurabilityBoundary::SourceObjects,
                        ..
                    },
                )
                | (3, Error::DetachedOutcomeUnknown { .. })
                | (
                    4,
                    Error::DetachedButNotSynced {
                        boundary: DurabilityBoundary::QuarantineState,
                        ..
                    },
                )
                | (
                    5,
                    Error::DetachedButNotSynced {
                        boundary: DurabilityBoundary::QuarantineMarkerStaging,
                        ..
                    },
                ) => {}
                (_, other) => panic!("unexpected typed fault: {other}"),
            }
            let object = app
                .join("recovery-snapshots/v1/vaults")
                .join(vault_id.to_string())
                .join("objects")
                .join(snapshot_id.to_string());
            assert!(!object.exists());
            store
                .quarantine_retention(
                    run_id,
                    1,
                    RetentionPolicy {
                        max_age_ms: 0,
                        max_per_lineage: usize::MAX,
                        max_logical_bytes: u64::MAX,
                    },
                )
                .expect("destination-only retry");
            assert!(!object.exists());
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn exactly_128_stable_runs_allow_existing_retry_but_reject_new_run() {
        let temporary = tempfile::tempdir().expect("temporary");
        let base = temporary.path().canonicalize().expect("canonical");
        let app = base.join("app");
        let vault = base.join("vault");
        fs::create_dir(&app).expect("app");
        fs::create_dir(&vault).expect("vault");
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).expect("private app");
        let vault_id = Uuid::new_v4();
        let store = SnapshotStore::open(&app, &vault, vault_id).expect("store");
        let (_runs_initial, _work, _) = store.open_run_roots().expect("roots");
        let runs_path = app
            .join("recovery-snapshots/v1/vaults")
            .join(vault_id.to_string())
            .join("quarantine/v1/runs");
        let requested = Uuid::from_u128(1);
        for value in 1_u128..=128 {
            let run_id = Uuid::from_u128(value);
            let snapshot_id = Uuid::from_u128(1000 + value);
            let payload = b"marked";
            let manifest = super::super::SnapshotManifest::new(
                snapshot_id,
                vault_id,
                "cap.md",
                0,
                super::super::SnapshotRevision::from_bytes(payload),
            )
            .expect("manifest");
            let manifest_bytes = serde_json::to_vec(&manifest).expect("manifest bytes");
            let lineage_key = "cap.md".to_owned();
            let mut plan = GcPlan {
                version: 1,
                run_id,
                vault_id,
                policy: RetentionPolicy::default(),
                evaluated_at_unix_ms: 0,
                candidates: vec![GcCandidate {
                    snapshot_id,
                    manifest_blake3: blake3::hash(&manifest_bytes).to_hex().to_string(),
                    payload_blake3: manifest.revision.blake3_hex.clone(),
                    payload_bytes: payload.len() as u64,
                    logical_bytes: manifest_bytes.len() as u64 + payload.len() as u64,
                    created_at_unix_ms: 0,
                    lineage_blake3: blake3::hash(lineage_key.as_bytes()).to_hex().to_string(),
                    lineage_key,
                    reasons: vec![RetentionReason::Age],
                }],
                plan_blake3: String::new(),
            };
            plan.plan_blake3 = plan_digest(&plan).expect("digest");
            let run = runs_path.join(run_id.to_string());
            fs::create_dir(&run).expect("run");
            fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).expect("private run");
            for child in [ITEMS, STATE, MARKER_STAGING] {
                fs::create_dir(run.join(child)).expect("child");
                fs::set_permissions(run.join(child), fs::Permissions::from_mode(0o700))
                    .expect("private child");
            }
            let item = run.join(ITEMS).join(snapshot_id.to_string());
            fs::create_dir(&item).expect("item");
            fs::set_permissions(&item, fs::Permissions::from_mode(0o700)).expect("private item");
            fs::write(item.join(MANIFEST_FILE), &manifest_bytes).expect("manifest");
            fs::write(item.join(super::super::PAYLOAD_FILE), payload).expect("payload");
            for file in [
                item.join(MANIFEST_FILE),
                item.join(super::super::PAYLOAD_FILE),
            ] {
                fs::set_permissions(file, fs::Permissions::from_mode(0o600)).expect("private file");
            }
            let marker = DetachedMarker {
                version: 1,
                run_id,
                snapshot_id,
                plan_blake3: plan.plan_blake3.clone(),
            };
            let marker_path = run.join(STATE).join(format!("{snapshot_id}.json"));
            fs::write(&marker_path, serde_json::to_vec(&marker).expect("marker"))
                .expect("marker file");
            fs::set_permissions(marker_path, fs::Permissions::from_mode(0o600))
                .expect("private marker");
            fs::write(run.join(PLAN), serde_json::to_vec(&plan).expect("plan")).expect("plan file");
            fs::set_permissions(run.join(PLAN), fs::Permissions::from_mode(0o600))
                .expect("private plan");
        }
        let report = store
            .quarantine_retention(requested, 0, RetentionPolicy::default())
            .expect("existing retry at cap");
        assert_eq!(report.outcome, QuarantineOutcome::RecoveredExisting);

        let snapshot_id = Uuid::new_v4();
        let payload = b"new candidate";
        let manifest = super::super::SnapshotManifest::new(
            snapshot_id,
            vault_id,
            "new.md",
            0,
            super::super::SnapshotRevision::from_bytes(payload),
        )
        .expect("manifest");
        store
            .publish(&manifest, payload)
            .expect("publish candidate");
        let new_run = Uuid::new_v4();
        assert!(matches!(
            store.quarantine_retention(
                new_run,
                1,
                RetentionPolicy {
                    max_age_ms: 0,
                    max_per_lineage: usize::MAX,
                    max_logical_bytes: u64::MAX,
                }
            ),
            Err(Error::TooManyGcRuns)
        ));
        assert!(!runs_path.join(new_run.to_string()).exists());
    }
}
