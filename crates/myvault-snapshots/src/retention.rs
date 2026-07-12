use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

use super::{
    inspect_object, open_optional_private_dir, Error, EvidenceLocation, SnapshotEvidence,
    SnapshotManifest, SnapshotStore,
};

pub const MAX_RETENTION_CANDIDATES: usize = 256;
pub const MAX_SNAPSHOT_SCAN_ENTRIES: usize = 8192;
pub const MAX_VERIFICATION_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const DEFAULT_MAX_LOGICAL_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub max_age_ms: u64,
    pub max_per_lineage: usize,
    pub max_logical_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age_ms: DEFAULT_MAX_AGE_MS,
            max_per_lineage: 100,
            max_logical_bytes: DEFAULT_MAX_LOGICAL_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RetentionReason {
    Age,
    LineageCount,
    TotalSize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionCandidate {
    pub snapshot_id: Uuid,
    pub path: String,
    pub created_at_unix_ms: u64,
    pub logical_bytes: u64,
    pub reasons: Vec<RetentionReason>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionPlan {
    pub candidates: Vec<RetentionCandidate>,
    pub scanned_entries: usize,
    pub supported_objects: usize,
    pub opaque_evidence: usize,
    pub staging_evidence: usize,
    pub supported_logical_bytes: u64,
    pub verified_bytes: u64,
    pub verification_budget_exhausted: bool,
    pub candidate_cap_reached: bool,
    /// False whenever opaque/staging evidence or a verification/candidate bound
    /// prevents proving that the configured capacity can be achieved.
    pub capacity_proven: bool,
}

impl RetentionPolicy {
    /// Pure deterministic retention selection over already validated manifests.
    /// Rename starts a new lineage because the lineage is the path collision key.
    ///
    /// # Errors
    /// Rejects any manifest that does not satisfy the current canonical schema.
    pub fn plan_manifests(
        self,
        now_unix_ms: u64,
        manifests: &[SnapshotManifest],
    ) -> Result<Vec<RetentionCandidate>, Error> {
        let mut snapshot_ids = BTreeSet::new();
        let expected_vault_id = manifests.first().map(|manifest| manifest.vault_id);
        for manifest in manifests {
            manifest.validate()?;
            if !snapshot_ids.insert(manifest.snapshot_id)
                || expected_vault_id.is_some_and(|vault_id| manifest.vault_id != vault_id)
            {
                return Err(Error::SnapshotCollision);
            }
        }
        let mut reasons = BTreeMap::<Uuid, BTreeSet<RetentionReason>>::new();
        let mut ordered = manifests.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|manifest| (manifest.created_at_unix_ms, manifest.snapshot_id));

        for manifest in &ordered {
            if now_unix_ms
                .checked_sub(self.max_age_ms)
                .is_some_and(|cutoff| manifest.created_at_unix_ms <= cutoff)
            {
                reasons
                    .entry(manifest.snapshot_id)
                    .or_default()
                    .insert(RetentionReason::Age);
            }
        }

        let mut lineages = BTreeMap::<String, Vec<&SnapshotManifest>>::new();
        for manifest in manifests {
            let lineage = myvault_core::VaultPath::from_portable(&manifest.path)
                .map_err(|_| Error::InvalidNotePath)?
                .collision_key();
            lineages.entry(lineage).or_default().push(manifest);
        }
        for lineage in lineages.values_mut() {
            lineage.sort_by_key(|manifest| (manifest.created_at_unix_ms, manifest.snapshot_id));
            let excess = lineage.len().saturating_sub(self.max_per_lineage);
            for manifest in lineage.iter().take(excess) {
                reasons
                    .entry(manifest.snapshot_id)
                    .or_default()
                    .insert(RetentionReason::LineageCount);
            }
        }

        let logical_sizes = manifests
            .iter()
            .map(|manifest| Ok((manifest.snapshot_id, manifest_logical_bytes(manifest)?)))
            .collect::<Result<BTreeMap<_, _>, Error>>()?;
        let mut remaining = checked_sum(logical_sizes.values().copied())?;
        for manifest in &ordered {
            if remaining <= self.max_logical_bytes {
                break;
            }
            reasons
                .entry(manifest.snapshot_id)
                .or_default()
                .insert(RetentionReason::TotalSize);
            remaining = remaining
                .checked_sub(logical_sizes[&manifest.snapshot_id])
                .ok_or(Error::ArithmeticOverflow)?;
        }

        Ok(ordered
            .into_iter()
            .filter_map(|manifest| {
                let entry_reasons = reasons.remove(&manifest.snapshot_id)?;
                Some(RetentionCandidate {
                    snapshot_id: manifest.snapshot_id,
                    path: manifest.path.clone(),
                    created_at_unix_ms: manifest.created_at_unix_ms,
                    logical_bytes: logical_sizes[&manifest.snapshot_id],
                    reasons: entry_reasons.into_iter().collect(),
                })
            })
            .collect())
    }
}

impl SnapshotStore {
    /// Produces a bounded deterministic dry-run retention report. This method
    /// never detaches, unlinks, quarantines, or modifies snapshot evidence;
    /// deletion is intentionally deferred to the next milestone.
    ///
    /// # Errors
    /// Fails closed on lock tampering, I/O errors, or more than 8192 physical
    /// entries across objects and staging.
    pub fn plan_retention(
        &self,
        now_unix_ms: u64,
        policy: RetentionPolicy,
    ) -> Result<RetentionPlan, Error> {
        let operation = self.lock_operation()?;
        let result = self.plan_retention_locked(now_unix_ms, policy);
        match (result, operation.finish()) {
            (Ok(plan), Ok(())) => Ok(plan),
            (Ok(_), Err(_)) => Err(Error::OperationLockLost),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(_)) => Err(Error::OperationFailedAndLockLost(Box::new(error))),
        }
    }

    fn plan_retention_locked(
        &self,
        now_unix_ms: u64,
        policy: RetentionPolicy,
    ) -> Result<RetentionPlan, Error> {
        let mut physical = Vec::<(EvidenceLocation, String)>::new();
        collect_entries(&self.objects, EvidenceLocation::Objects, &mut physical)?;
        collect_entries(&self.staging, EvidenceLocation::Staging, &mut physical)?;
        physical.sort_by(|left, right| {
            (location_order(left.0), &left.1).cmp(&(location_order(right.0), &right.1))
        });

        let scanned_entries = physical.len();
        let mut by_id = BTreeMap::<Uuid, Vec<EvidenceLocation>>::new();
        let mut opaque = 0_usize;
        for (location, name) in &physical {
            if *location == EvidenceLocation::Staging && name.starts_with(".work-") {
                opaque += 1;
                continue;
            }
            match canonical_snapshot_id(name) {
                Some(id) => by_id.entry(id).or_default().push(*location),
                None => opaque += 1,
            }
        }

        let mut manifests = Vec::new();
        let mut staging_evidence = 0_usize;
        let mut verified_bytes = 0_u64;
        let mut budget_exhausted = false;
        for (snapshot_id, locations) in by_id {
            if locations.len() != 1 {
                opaque += locations.len();
                continue;
            }
            let location = locations[0];
            let parent = match location {
                EvidenceLocation::Objects => &self.objects,
                EvidenceLocation::Staging => &self.staging,
            };
            let name = snapshot_id.to_string();
            let Some(directory) = open_optional_private_dir(parent, &name).ok().flatten() else {
                opaque += 1;
                continue;
            };
            let mut remaining_budget = MAX_VERIFICATION_BYTES
                .checked_sub(verified_bytes)
                .ok_or(Error::ArithmeticOverflow)?;
            let inspection = inspect_object(
                &directory,
                location,
                snapshot_id,
                self.vault_id,
                Some(&mut remaining_budget),
            );
            verified_bytes = MAX_VERIFICATION_BYTES
                .checked_sub(remaining_budget)
                .ok_or(Error::ArithmeticOverflow)?;
            match inspection {
                Ok((Ok(SnapshotEvidence::Supported { manifest, .. }), logical_bytes)) => {
                    debug_assert!(logical_bytes <= verified_bytes);
                    if location == EvidenceLocation::Objects {
                        manifests.push(manifest);
                    } else {
                        staging_evidence += 1;
                    }
                }
                Ok((Ok(SnapshotEvidence::Unsupported { .. }) | Err(_), logical_bytes)) => {
                    debug_assert!(logical_bytes <= verified_bytes);
                    opaque += 1;
                }
                Err(Error::VerificationBudgetExceeded) => {
                    budget_exhausted = true;
                    opaque += 1;
                }
                Err(_) => opaque += 1,
            }
        }

        let supported_logical_bytes = checked_sum(
            manifests
                .iter()
                .map(manifest_logical_bytes)
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        let supported_objects = manifests.len();
        let all_candidates = policy.plan_manifests(now_unix_ms, &manifests)?;
        let candidate_cap_reached = all_candidates.len() > MAX_RETENTION_CANDIDATES;
        let candidates = all_candidates
            .into_iter()
            .take(MAX_RETENTION_CANDIDATES)
            .collect();
        let capacity_proven =
            opaque == 0 && staging_evidence == 0 && !budget_exhausted && !candidate_cap_reached;

        Ok(RetentionPlan {
            candidates,
            scanned_entries,
            supported_objects,
            opaque_evidence: opaque,
            staging_evidence,
            supported_logical_bytes,
            verified_bytes,
            verification_budget_exhausted: budget_exhausted,
            candidate_cap_reached,
            capacity_proven,
        })
    }
}

fn manifest_logical_bytes(manifest: &SnapshotManifest) -> Result<u64, Error> {
    let manifest_bytes = u64::try_from(super::canonical_manifest_bytes(manifest)?.len())
        .map_err(|_| Error::ArithmeticOverflow)?;
    manifest_bytes
        .checked_add(manifest.revision.byte_len)
        .ok_or(Error::ArithmeticOverflow)
}

fn checked_sum(values: impl IntoIterator<Item = u64>) -> Result<u64, Error> {
    values.into_iter().try_fold(0_u64, |total, bytes| {
        total.checked_add(bytes).ok_or(Error::ArithmeticOverflow)
    })
}

fn collect_entries(
    directory: &cap_std::fs::Dir,
    location: EvidenceLocation,
    entries: &mut Vec<(EvidenceLocation, String)>,
) -> Result<(), Error> {
    for entry in directory.entries()? {
        if entries.len() == MAX_SNAPSHOT_SCAN_ENTRIES {
            return Err(Error::TooManySnapshotEntries);
        }
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            // Preserve a deterministic opaque placeholder without exposing raw bytes.
            entries.push((location, format!("<non-utf8-{}>", entries.len())));
            continue;
        };
        entries.push((location, name));
    }
    Ok(())
}

fn canonical_snapshot_id(name: &str) -> Option<Uuid> {
    let id = Uuid::parse_str(name).ok()?;
    (!id.is_nil() && id.to_string() == name).then_some(id)
}

const fn location_order(location: EvidenceLocation) -> u8 {
    match location {
        EvidenceLocation::Objects => 0,
        EvidenceLocation::Staging => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{checked_sum, Error};

    #[test]
    fn checked_logical_sum_reports_overflow() {
        assert!(matches!(
            checked_sum([u64::MAX, 1]),
            Err(Error::ArithmeticOverflow)
        ));
    }
}
