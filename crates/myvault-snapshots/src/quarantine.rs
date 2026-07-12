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
const ITEMS: &str = "items";
const STATE: &str = "state";
const MARKER_STAGING: &str = "marker-staging";
const MAX_PLAN_BYTES: u64 = 128 * 1024;
const MAX_RUNS: usize = 128;

#[cfg(test)]
static DETACH_FAULT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

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
        if bounded_entry_count(&runs)?
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
        for entry in runs.entries()? {
            if names.len() == MAX_RUNS {
                return Err(Error::TooManyGcRuns);
            }
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or(Error::InvalidGcPlan)?
                .to_owned();
            let id = Uuid::parse_str(&name).map_err(|_| Error::InvalidGcPlan)?;
            if id.is_nil() || id.to_string() != name {
                return Err(Error::InvalidGcPlan);
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
    let mut names = run
        .entries()?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod fault_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn post_rename_faults_preserve_detached_fact_and_retry_destination_only() {
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
