//! Capability-relative immutable witnesses for local sync execution.
//!
//! This is deliberately a separate namespace from `myvault-recovery`.  A
//! witness is crash evidence only: it cannot recreate verifier evidence or
//! authorize a mutation or final recovery decision.

use crate::{Error, LocalExecutionOutcome, Result};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use myvault_private_fs as private_fs;
use std::collections::BTreeSet;
#[cfg(windows)]
use std::ffi::OsStr;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use uuid::Uuid;

use crate::store::PrivateStoragePolicy;

pub(crate) const JOURNAL_DIRECTORY: &str = "sync-execution-journal-v1";
const MAX_WITNESS_BYTES: u64 = 512;
const MAGIC: &[u8; 6] = b"MVSEJ\0";
const VERSION: u8 = 2;
const PRE_SIDE_EFFECT: u8 = 1;
const OUTCOME: u8 = 2;
/// A receipt-consumption proof is intentionally a separate immutable journal
/// object.  `SQLite` can retain a receipt only while it is needed by the active
/// batch; this proof remains after batch cleanup and therefore prevents a
/// later receipt deletion from being reinterpreted as a pre-receipt crash.
const BRIDGE_CONSUMPTION: u8 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreSideEffectWitness {
    pub(crate) operation_id: Uuid,
    pub(crate) attempt_number: u32,
    pub(crate) boundary_id: Uuid,
    pub(crate) boundary_occurred_at_unix_ms: u64,
    pub(crate) intent_fingerprint: [u8; 32],
    pub(crate) contract_fingerprint: [u8; 32],
    pub(crate) collision_snapshot_fingerprint: [u8; 32],
    pub(crate) created_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutcomeWitness {
    pub(crate) pre: PreSideEffectWitness,
    pub(crate) outcome_id: Uuid,
    pub(crate) evidence_id: Uuid,
    pub(crate) outcome: LocalExecutionOutcome,
    pub(crate) evidence_fingerprint: [u8; 32],
    /// Present only for a verifier-derived outcome that is later eligible for
    /// the R3.5 bridge.  This is deliberately independent of the local
    /// classifier fingerprint, which merely *includes* this R3 fact.
    pub(crate) r3_mutation_evidence_fingerprint: Option<[u8; 32]>,
    pub(crate) created_at_unix_ms: u64,
}

/// Immutable proof that one exact `SQLite` bridge receipt was committed and
/// consumed by the R3.5 bridge.  It is published only after the receipt and
/// dependency transaction has committed.  Its redundancy is deliberate: the
/// receipt fingerprint seals the complete receipt preimage, while these
/// fields make the journal object self-describing and bind it to the sealed
/// local/R3 outcome without trusting an `SQLite` lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BridgeConsumptionWitness {
    pub(crate) pre: PreSideEffectWitness,
    pub(crate) receipt_id: Uuid,
    pub(crate) receipt_fingerprint: [u8; 32],
    pub(crate) outcome_id: Uuid,
    pub(crate) evidence_id: Uuid,
    pub(crate) local_evidence_fingerprint: [u8; 32],
    pub(crate) outcome_occurred_at_unix_ms: u64,
    pub(crate) r3_intent_fingerprint: [u8; 32],
    pub(crate) r3_evidence_fingerprint: [u8; 32],
    pub(crate) dependency_kind: u8,
}

pub(crate) struct SyncExecutionJournal {
    root: Dir,
    canonical_root: PathBuf,
    root_identity: private_fs::HeldDirectoryIdentity,
    parent: Dir,
    parent_identity: private_fs::HeldDirectoryIdentity,
    vault_id: Uuid,
    directory: Dir,
    directory_identity: private_fs::HeldDirectoryIdentity,
    policy: PrivateStoragePolicy,
    /// A rename that is visible but whose directory sync failed is not cursor
    /// authority in this live process.  An exact retry must perform the
    /// directory-sync repair before the marker can be consumed.
    unconfirmed_consumptions: Mutex<BTreeSet<(Uuid, u32)>>,
    #[cfg(test)]
    fail_next_directory_sync: AtomicBool,
    #[cfg(test)]
    fail_before_next_file_sync: AtomicBool,
    #[cfg(test)]
    fail_next_file_sync: AtomicBool,
    #[cfg(test)]
    replace_source_before_next_rename: AtomicBool,
    #[cfg(test)]
    replace_named_file_after_next_read: AtomicBool,
    #[cfg(test)]
    opened_existing_temp_for_sync: AtomicBool,
    /// Test-only liveness observations for the source handle that supplies the
    /// identity used after publication.  The bits are set immediately before
    /// rename, immediately after rename, and during final verification.
    #[cfg(test)]
    held_source_liveness_observations: AtomicU8,
}

impl SyncExecutionJournal {
    pub(crate) fn open(
        root: &Dir,
        canonical_root: &Path,
        parent: &Dir,
        vault_id: Uuid,
        policy: PrivateStoragePolicy,
    ) -> Result<Self> {
        let directory =
            super::store::create_or_open_storage_dir(parent, JOURNAL_DIRECTORY, policy)?;
        let directory_identity = private_fs::held_directory_identity(&directory)?;
        let journal = Self {
            root: root.try_clone()?,
            canonical_root: canonical_root.to_owned(),
            root_identity: private_fs::held_directory_identity(root)?,
            parent: parent.try_clone()?,
            parent_identity: private_fs::held_directory_identity(parent)?,
            vault_id,
            directory,
            directory_identity,
            policy,
            unconfirmed_consumptions: Mutex::new(BTreeSet::new()),
            #[cfg(test)]
            fail_next_directory_sync: AtomicBool::new(false),
            #[cfg(test)]
            fail_before_next_file_sync: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_file_sync: AtomicBool::new(false),
            #[cfg(test)]
            replace_source_before_next_rename: AtomicBool::new(false),
            #[cfg(test)]
            replace_named_file_after_next_read: AtomicBool::new(false),
            #[cfg(test)]
            opened_existing_temp_for_sync: AtomicBool::new(false),
            #[cfg(test)]
            held_source_liveness_observations: AtomicU8::new(0),
        };
        journal.check_directory()?;
        // A visible no-replace entry is not durable merely because this new
        // process can read it.  Every open establishes a fresh durability
        // boundary for the journal directory before any existing marker can
        // be consumed as cursor evidence.  If the platform cannot sync the
        // directory, opening fails closed rather than forgetting the
        // previous process's failed sync in volatile memory.
        journal.sync_published_directory()?;
        journal.check_directory()?;
        Ok(journal)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_directory_sync_for_test(&self) {
        self.fail_next_directory_sync.store(true, Ordering::SeqCst);
    }

    /// Simulates a crash after exact temp bytes are present but before their
    /// first file durability barrier.  It is deliberately test-only: callers
    /// cannot gain a production publication bypass from this fault point.
    #[cfg(test)]
    pub(crate) fn fail_before_next_file_sync_for_test(&self) {
        self.fail_before_next_file_sync
            .store(true, Ordering::SeqCst);
    }

    /// Injects the file-sync barrier itself.  Tests use this to prove an
    /// already-existing exact deterministic temp is synced again before it
    /// can be renamed.
    #[cfg(test)]
    pub(crate) fn fail_next_file_sync_for_test(&self) {
        self.fail_next_file_sync.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn file_sync_test_faults_consumed(&self) -> bool {
        !self.fail_before_next_file_sync.load(Ordering::SeqCst)
            && !self.fail_next_file_sync.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn replace_source_before_next_rename_for_test(&self) {
        self.replace_source_before_next_rename
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn replace_named_file_after_next_read_for_test(&self) {
        self.replace_named_file_after_next_read
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn replacement_test_faults_consumed(&self) -> bool {
        !self
            .replace_source_before_next_rename
            .load(Ordering::SeqCst)
            && !self
                .replace_named_file_after_next_read
                .load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn existing_temp_rw_opened_for_sync_for_test(&self) -> bool {
        self.opened_existing_temp_for_sync.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn held_source_liveness_observations_for_test(&self) -> u8 {
        self.held_source_liveness_observations
            .load(Ordering::SeqCst)
    }

    pub(crate) fn publish_pre(&self, witness: &PreSideEffectWitness) -> Result<bool> {
        let bytes = encode_pre(witness);
        self.publish(
            &pre_name(witness.operation_id, witness.attempt_number),
            &bytes,
        )
    }

    pub(crate) fn publish_outcome(&self, witness: &OutcomeWitness) -> Result<bool> {
        let bytes = encode_outcome(witness);
        self.publish(
            &outcome_name(witness.pre.operation_id, witness.pre.attempt_number),
            &bytes,
        )
    }

    pub(crate) fn publish_bridge_consumption(
        &self,
        witness: &BridgeConsumptionWitness,
    ) -> Result<bool> {
        let bytes = encode_bridge_consumption(witness);
        let key = (witness.pre.operation_id, witness.pre.attempt_number);
        match self.publish(
            &bridge_consumption_name(witness.pre.operation_id, witness.pre.attempt_number),
            &bytes,
        ) {
            Ok(published) => {
                self.unconfirmed_consumptions
                    .lock()
                    .map_err(|_| Error::LocalExecutionJournalMismatch)?
                    .remove(&key);
                Ok(published)
            }
            Err(error @ Error::LocalExecutionJournalPublishedButNotSynced(_)) => {
                self.unconfirmed_consumptions
                    .lock()
                    .map_err(|_| Error::LocalExecutionJournalMismatch)?
                    .insert(key);
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn read_pre(
        &self,
        operation_id: Uuid,
        attempt_number: u32,
    ) -> Result<Option<PreSideEffectWitness>> {
        let name = pre_name(operation_id, attempt_number);
        let Some(bytes) = self.read_if_exists(&name)? else {
            return Ok(None);
        };
        let witness = decode_pre(&bytes)?;
        if witness.operation_id != operation_id || witness.attempt_number != attempt_number {
            return Err(Error::LocalExecutionJournalMalformed);
        }
        Ok(Some(witness))
    }

    pub(crate) fn read_outcome(
        &self,
        operation_id: Uuid,
        attempt_number: u32,
    ) -> Result<Option<OutcomeWitness>> {
        let name = outcome_name(operation_id, attempt_number);
        let Some(bytes) = self.read_if_exists(&name)? else {
            return Ok(None);
        };
        let witness = decode_outcome(&bytes)?;
        if witness.pre.operation_id != operation_id || witness.pre.attempt_number != attempt_number
        {
            return Err(Error::LocalExecutionJournalMalformed);
        }
        Ok(Some(witness))
    }

    pub(crate) fn read_bridge_consumption(
        &self,
        operation_id: Uuid,
        attempt_number: u32,
    ) -> Result<Option<BridgeConsumptionWitness>> {
        let name = bridge_consumption_name(operation_id, attempt_number);
        let Some(bytes) = self.read_if_exists(&name)? else {
            return Ok(None);
        };
        let witness = decode_bridge_consumption(&bytes)?;
        if witness.pre.operation_id != operation_id || witness.pre.attempt_number != attempt_number
        {
            return Err(Error::LocalExecutionJournalMalformed);
        }
        Ok(Some(witness))
    }

    pub(crate) fn bridge_consumption_is_confirmed(
        &self,
        operation_id: Uuid,
        attempt_number: u32,
    ) -> Result<bool> {
        Ok(!self
            .unconfirmed_consumptions
            .lock()
            .map_err(|_| Error::LocalExecutionJournalMismatch)?
            .contains(&(operation_id, attempt_number)))
    }

    fn publish(&self, final_name: &str, bytes: &[u8]) -> Result<bool> {
        self.check_directory()?;
        // Exact retries are common after a crash.  Check capability-relative
        // final state before allocating a temp so an idempotent retry produces
        // no new forensic artifact.  A later rename race still retains the
        // temp created by this publisher (below).
        match self.read_raw(final_name) {
            Ok(actual) if actual == bytes => {
                // A prior publisher may have completed the rename and then
                // crashed (or faulted) before syncing the containing
                // directory.  An exact retry owns the durability repair even
                // though it creates no temporary forensic file.
                self.check_directory()?;
                self.sync_published_directory()?;
                self.check_directory()?;
                return Ok(false);
            }
            Ok(_) => return Err(Error::LocalExecutionJournalCollision),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // One bounded, deterministic staging pathname belongs to each final
        // witness.  A crash before rename therefore cannot create an
        // unbounded collection of random forensic files.  We never unlink a
        // pre-existing staging file: it must be the exact canonical bytes or
        // publication fails closed and preserves the evidence for inspection.
        let temporary_name = format!(".sync-execution-witness-{final_name}.tmp");
        let (mut temporary, temporary_identity) =
            self.open_or_create_exact_temp(&temporary_name, bytes)?;
        // A directory fsync cannot make data that was never file-synced
        // durable.  This barrier is required for both a newly-created temp
        // and an exact deterministic temp recovered after a crash.
        #[cfg(test)]
        if self
            .fail_before_next_file_sync
            .swap(false, Ordering::SeqCst)
        {
            return Err(Error::Io(io::Error::other(
                "injected journal write-before-file-sync crash",
            )));
        }
        #[cfg(test)]
        if self.fail_next_file_sync.swap(false, Ordering::SeqCst) {
            return Err(Error::Io(io::Error::other(
                "injected journal file sync failure",
            )));
        }
        temporary.sync_all()?;
        self.read_bound_exact(&mut temporary, &temporary_identity, &temporary_name, bytes)?;
        // `FileIdentity` is evidence only while the source handle that yielded
        // it remains held.  Keep that exact synced handle alive through the
        // no-replace rename, directory durability barrier, and both final
        // pathname/identity checks; only then may it be closed.
        self.check_directory()?;
        #[cfg(test)]
        if self
            .replace_source_before_next_rename
            .swap(false, Ordering::SeqCst)
        {
            self.replace_named_file_for_test(&temporary_name)?;
        }

        #[cfg(test)]
        self.observe_held_source_liveness_for_test(&temporary, &temporary_identity, 0b001)?;
        let rename_result = atomic_rename_noreplace(&self.directory, &temporary_name, final_name);
        #[cfg(test)]
        self.observe_held_source_liveness_for_test(&temporary, &temporary_identity, 0b010)?;
        let published = match rename_result {
            Ok(()) => true,
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                if self.named_file_is_exact(final_name, bytes, None)? {
                    // A competing publisher won after this call created its
                    // unique temp.  Preserve our temp rather than unlinking a
                    // pathname in a race: it is bounded private recovery/
                    // forensics evidence and cannot affect the exact final.
                    false
                } else {
                    return Err(Error::LocalExecutionJournalCollision);
                }
            }
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                // The source may have been renamed by a racing publisher
                // after its final check.  Only an exact final is recoverable;
                // a missing or substituted final remains fail-closed.
                if self.named_file_is_exact(final_name, bytes, None)? {
                    false
                } else {
                    return Err(Error::LocalExecutionJournalMismatch);
                }
            }
            Err(error) => return Err(error),
        };
        // The source pathname was deliberately never trusted after we opened
        // it.  Bind the resulting final name to that exact held file and read
        // it again before declaring publication successful.
        Self::validate_held_source_identity(&temporary, &temporary_identity)?;
        if published && !self.named_file_is_exact(final_name, bytes, Some(&temporary_identity))? {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        self.check_directory()?;
        self.sync_published_directory()?;
        self.check_directory()?;
        // Directory sync may block long enough for a hostile pathname swap.
        // Never return success until the final pathname still resolves to the
        // exact file that was synced before the no-replace rename.
        Self::validate_held_source_identity(&temporary, &temporary_identity)?;
        if published && !self.named_file_is_exact(final_name, bytes, Some(&temporary_identity))? {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        #[cfg(test)]
        self.observe_held_source_liveness_for_test(&temporary, &temporary_identity, 0b100)?;
        drop(temporary);
        Ok(published)
    }

    fn read_if_exists(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.check_directory()?;
        match self.read_raw(name) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn read_raw(&self, name: &str) -> Result<Vec<u8>> {
        let metadata = self.directory.symlink_metadata(name)?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_WITNESS_BYTES {
            return Err(Error::LocalExecutionJournalMalformed);
        }
        let mut file =
            super::store::open_existing_storage_file(&self.directory, name, self.policy)?;
        let identity = myvault_platform_fs::file_identity(&file)?;
        self.read_bound_bytes(&mut file, &identity, name)
    }

    fn open_or_create_exact_temp(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<(cap_std::fs::File, myvault_platform_fs::FileIdentity)> {
        for _ in 0..2 {
            match self.open_bound_exact(name, bytes) {
                Ok(value) => return Ok(value),
                Err(error) if is_not_found(&error) => {}
                Err(error) => return Err(error),
            }
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            match self.directory.open_with(name, &options) {
                Ok(mut file) => {
                    super::store::harden_new_storage_file(&file, self.policy)?;
                    file.write_all(bytes)?;
                    super::store::verify_storage_file(&file, self.policy)?;
                    let identity = myvault_platform_fs::file_identity(&file)?;
                    return Ok((file, identity));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(Error::Io(error)),
            }
        }
        self.open_bound_exact(name, bytes)
    }

    fn open_bound_exact(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<(cap_std::fs::File, myvault_platform_fs::FileIdentity)> {
        // An exact crash-recovered temp is about to cross `sync_all`; opening
        // it read-only works on Unix but cannot FlushFileBuffers on Windows.
        // This dedicated private/no-follow RW opener does not weaken ordinary
        // final-marker reads, which remain read-only in `read_raw`.
        let mut file = super::store::open_existing_storage_file_read_write(
            &self.directory,
            name,
            self.policy,
        )?;
        #[cfg(test)]
        self.opened_existing_temp_for_sync
            .store(true, Ordering::SeqCst);
        let identity = myvault_platform_fs::file_identity(&file)?;
        self.read_bound_exact(&mut file, &identity, name, bytes)?;
        Ok((file, identity))
    }

    fn read_bound_exact(
        &self,
        file: &mut cap_std::fs::File,
        identity: &myvault_platform_fs::FileIdentity,
        name: &str,
        expected: &[u8],
    ) -> Result<()> {
        if self.read_bound_bytes(file, identity, name)? != expected {
            return Err(Error::LocalExecutionJournalCollision);
        }
        Ok(())
    }

    fn read_bound_bytes(
        &self,
        file: &mut cap_std::fs::File,
        identity: &myvault_platform_fs::FileIdentity,
        name: &str,
    ) -> Result<Vec<u8>> {
        super::store::verify_storage_file(file, self.policy)?;
        if &myvault_platform_fs::file_identity(file)? != identity {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        let length = file.metadata()?.len();
        if length > MAX_WITNESS_BYTES {
            return Err(Error::LocalExecutionJournalMalformed);
        }
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(length).map_err(|_| Error::LocalExecutionJournalMalformed)?,
        );
        file.take(MAX_WITNESS_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_WITNESS_BYTES {
            return Err(Error::LocalExecutionJournalMalformed);
        }
        super::store::verify_storage_file(file, self.policy)?;
        if &myvault_platform_fs::file_identity(file)? != identity
            || !self.named_file_is_exact(name, &bytes, Some(identity))?
        {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        self.check_directory()?;
        Ok(bytes)
    }

    fn named_file_is_exact(
        &self,
        name: &str,
        expected: &[u8],
        expected_identity: Option<&myvault_platform_fs::FileIdentity>,
    ) -> Result<bool> {
        match super::store::open_existing_storage_file(&self.directory, name, self.policy) {
            Ok(mut file) => {
                let identity = myvault_platform_fs::file_identity(&file)?;
                if expected_identity.is_some_and(|value| value != &identity) {
                    return Ok(false);
                }
                let bytes = self.read_bound_bytes_without_reopen(&mut file, &identity)?;
                // A handle proves only what was opened, not what the pathname
                // resolves to now.  Re-open the capability-relative name and
                // compare its stable identity after reading so read_raw and
                // publication cannot accept a pathname/handle substitution.
                #[cfg(test)]
                if self
                    .replace_named_file_after_next_read
                    .swap(false, Ordering::SeqCst)
                {
                    self.replace_named_file_for_test(name)?;
                }
                let reopened = match super::store::open_existing_storage_file(
                    &self.directory,
                    name,
                    self.policy,
                ) {
                    Ok(file) => file,
                    Err(error) if is_not_found(&error) => return Ok(false),
                    Err(error) => return Err(error),
                };
                if myvault_platform_fs::file_identity(&reopened)? != identity {
                    return Ok(false);
                }
                Ok(bytes == expected)
            }
            Err(error) if is_not_found(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn read_bound_bytes_without_reopen(
        &self,
        file: &mut cap_std::fs::File,
        identity: &myvault_platform_fs::FileIdentity,
    ) -> Result<Vec<u8>> {
        super::store::verify_storage_file(file, self.policy)?;
        if &myvault_platform_fs::file_identity(file)? != identity {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        let length = file.metadata()?.len();
        if length > MAX_WITNESS_BYTES {
            return Err(Error::LocalExecutionJournalMalformed);
        }
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(length).map_err(|_| Error::LocalExecutionJournalMalformed)?,
        );
        file.take(MAX_WITNESS_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_WITNESS_BYTES {
            return Err(Error::LocalExecutionJournalMalformed);
        }
        super::store::verify_storage_file(file, self.policy)?;
        if &myvault_platform_fs::file_identity(file)? != identity {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        Ok(bytes)
    }

    fn validate_held_source_identity(
        file: &cap_std::fs::File,
        expected_identity: &myvault_platform_fs::FileIdentity,
    ) -> Result<()> {
        // The held source may legitimately have no directory link after an
        // adversarial pathname substitution.  Revalidate the handle identity
        // itself here and let the final capability-relative pathname check
        // report that substitution as the public mismatch.
        if &myvault_platform_fs::file_identity(file)? != expected_identity {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        Ok(())
    }

    #[cfg(test)]
    fn observe_held_source_liveness_for_test(
        &self,
        file: &cap_std::fs::File,
        expected_identity: &myvault_platform_fs::FileIdentity,
        observation: u8,
    ) -> Result<()> {
        // Metadata plus a freshly-read held-handle identity makes this a
        // deterministic proof that the source handle, not a stale identity
        // value, remains available at each publication boundary.
        let _ = file.metadata()?;
        Self::validate_held_source_identity(file, expected_identity)?;
        self.held_source_liveness_observations
            .fetch_or(observation, Ordering::SeqCst);
        Ok(())
    }

    #[cfg(test)]
    fn replace_named_file_for_test(&self, name: &str) -> Result<()> {
        self.directory.remove_file(name)?;
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut replacement = self.directory.open_with(name, &options)?;
        super::store::harden_new_storage_file(&replacement, self.policy)?;
        replacement.write_all(b"journal-test-substitution")?;
        replacement.sync_all()?;
        Ok(())
    }

    fn check_directory(&self) -> Result<()> {
        let canonical = self.canonical_root.canonicalize()?;
        if canonical != self.canonical_root {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        let reopened_root = Dir::open_ambient_dir(&self.canonical_root, ambient_authority())?;
        if private_fs::held_directory_identity(&self.root)? != self.root_identity
            || private_fs::held_directory_identity(&reopened_root)? != self.root_identity
        {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        let sync_root = super::store::open_existing_storage_dir(
            &reopened_root,
            super::store::ROOT_DIRECTORY,
            self.policy,
        )?;
        let version = super::store::open_existing_storage_dir(
            &sync_root,
            super::store::VERSION_DIRECTORY,
            self.policy,
        )?;
        let vaults = super::store::open_existing_storage_dir(
            &version,
            super::store::VAULTS_DIRECTORY,
            self.policy,
        )?;
        let vault_id = self.vault_id.to_string();
        let reopened_parent =
            super::store::open_existing_storage_dir(&vaults, &vault_id, self.policy)?;
        if private_fs::held_directory_identity(&self.parent)? != self.parent_identity
            || private_fs::held_directory_identity(&reopened_parent)? != self.parent_identity
        {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        let reopened = super::store::open_existing_storage_dir(
            &reopened_parent,
            JOURNAL_DIRECTORY,
            self.policy,
        )?;
        if private_fs::held_directory_identity(&self.directory)? != self.directory_identity
            || private_fs::held_directory_identity(&reopened)? != self.directory_identity
        {
            return Err(Error::LocalExecutionJournalMismatch);
        }
        Ok(())
    }

    fn sync_published_directory(&self) -> Result<()> {
        #[cfg(test)]
        if self.fail_next_directory_sync.swap(false, Ordering::SeqCst) {
            return Err(Error::LocalExecutionJournalPublishedButNotSynced(
                io::Error::other("injected directory sync failure"),
            ));
        }
        private_fs::sync_directory(&self.directory).map_err(|error| match error {
            private_fs::Error::Io(error) | private_fs::Error::DirectorySyncUnsupported(error) => {
                Error::LocalExecutionJournalPublishedButNotSynced(error)
            }
            other => Error::PrivateStorage(other),
        })
    }
}

fn is_not_found(error: &Error) -> bool {
    match error {
        Error::Io(value) => value.kind() == io::ErrorKind::NotFound,
        Error::PrivateStorage(private_fs::Error::Io(value)) => {
            value.kind() == io::ErrorKind::NotFound
        }
        _ => false,
    }
}

fn pre_name(operation_id: Uuid, attempt_number: u32) -> String {
    format!("{operation_id}-{attempt_number}.pre")
}

fn outcome_name(operation_id: Uuid, attempt_number: u32) -> String {
    format!("{operation_id}-{attempt_number}.out")
}

fn bridge_consumption_name(operation_id: Uuid, attempt_number: u32) -> String {
    format!("{operation_id}-{attempt_number}.bridge")
}

fn encode_pre(witness: &PreSideEffectWitness) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(156);
    encode_common(&mut bytes, PRE_SIDE_EFFECT, witness);
    bytes
}

fn encode_outcome(witness: &OutcomeWitness) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(254);
    encode_common(&mut bytes, OUTCOME, &witness.pre);
    bytes.extend_from_slice(witness.outcome_id.as_bytes());
    bytes.extend_from_slice(witness.evidence_id.as_bytes());
    bytes.push(encode_outcome_kind(witness.outcome));
    bytes.extend_from_slice(&witness.evidence_fingerprint);
    match witness.r3_mutation_evidence_fingerprint {
        Some(fingerprint) => {
            bytes.push(1);
            bytes.extend_from_slice(&fingerprint);
        }
        None => bytes.push(0),
    }
    bytes.extend_from_slice(&witness.created_at_unix_ms.to_be_bytes());
    bytes
}

fn encode_bridge_consumption(witness: &BridgeConsumptionWitness) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(367);
    encode_common(&mut bytes, BRIDGE_CONSUMPTION, &witness.pre);
    bytes.extend_from_slice(witness.receipt_id.as_bytes());
    bytes.extend_from_slice(&witness.receipt_fingerprint);
    bytes.extend_from_slice(witness.outcome_id.as_bytes());
    bytes.extend_from_slice(witness.evidence_id.as_bytes());
    bytes.extend_from_slice(&witness.local_evidence_fingerprint);
    bytes.extend_from_slice(&witness.outcome_occurred_at_unix_ms.to_be_bytes());
    bytes.extend_from_slice(&witness.r3_intent_fingerprint);
    bytes.extend_from_slice(&witness.r3_evidence_fingerprint);
    bytes.push(witness.dependency_kind);
    bytes
}

// Store-level forensic regressions need to replace an immutable file with a
// different *canonical* witness.  Keeping this test-only avoids giving normal
// callers any journal encoding capability.
#[cfg(test)]
pub(crate) fn canonical_pre_bytes_for_test(witness: &PreSideEffectWitness) -> Vec<u8> {
    encode_pre(witness)
}

#[cfg(test)]
pub(crate) fn canonical_outcome_bytes_for_test(witness: &OutcomeWitness) -> Vec<u8> {
    encode_outcome(witness)
}

#[cfg(test)]
pub(crate) fn canonical_bridge_consumption_bytes_for_test(
    witness: &BridgeConsumptionWitness,
) -> Vec<u8> {
    encode_bridge_consumption(witness)
}

fn encode_common(bytes: &mut Vec<u8>, kind: u8, witness: &PreSideEffectWitness) {
    bytes.extend_from_slice(MAGIC);
    bytes.push(VERSION);
    bytes.push(kind);
    bytes.extend_from_slice(witness.operation_id.as_bytes());
    bytes.extend_from_slice(&witness.attempt_number.to_be_bytes());
    bytes.extend_from_slice(witness.boundary_id.as_bytes());
    bytes.extend_from_slice(&witness.boundary_occurred_at_unix_ms.to_be_bytes());
    bytes.extend_from_slice(&witness.intent_fingerprint);
    bytes.extend_from_slice(&witness.contract_fingerprint);
    bytes.extend_from_slice(&witness.collision_snapshot_fingerprint);
    bytes.extend_from_slice(&witness.created_at_unix_ms.to_be_bytes());
}

fn decode_pre(bytes: &[u8]) -> Result<PreSideEffectWitness> {
    let (pre, cursor) = decode_common(bytes, PRE_SIDE_EFFECT)?;
    if cursor != bytes.len() || encode_pre(&pre) != bytes {
        return Err(Error::LocalExecutionJournalMalformed);
    }
    Ok(pre)
}

fn decode_outcome(bytes: &[u8]) -> Result<OutcomeWitness> {
    let (pre, mut cursor) = decode_common(bytes, OUTCOME)?;
    let outcome_id = read_uuid(bytes, &mut cursor)?;
    let evidence_id = read_uuid(bytes, &mut cursor)?;
    let outcome = decode_outcome_kind(read_u8(bytes, &mut cursor)?)?;
    let evidence_fingerprint = read_array_32(bytes, &mut cursor)?;
    let r3_mutation_evidence_fingerprint = match read_u8(bytes, &mut cursor)? {
        0 => None,
        1 => Some(read_array_32(bytes, &mut cursor)?),
        _ => return Err(Error::LocalExecutionJournalMalformed),
    };
    let created_at_unix_ms = read_u64(bytes, &mut cursor)?;
    let witness = OutcomeWitness {
        pre,
        outcome_id,
        evidence_id,
        outcome,
        evidence_fingerprint,
        r3_mutation_evidence_fingerprint,
        created_at_unix_ms,
    };
    if cursor != bytes.len() || encode_outcome(&witness) != bytes {
        return Err(Error::LocalExecutionJournalMalformed);
    }
    Ok(witness)
}

fn decode_bridge_consumption(bytes: &[u8]) -> Result<BridgeConsumptionWitness> {
    let (pre, mut cursor) = decode_common(bytes, BRIDGE_CONSUMPTION)?;
    let witness = BridgeConsumptionWitness {
        pre,
        receipt_id: read_uuid(bytes, &mut cursor)?,
        receipt_fingerprint: read_array_32(bytes, &mut cursor)?,
        outcome_id: read_uuid(bytes, &mut cursor)?,
        evidence_id: read_uuid(bytes, &mut cursor)?,
        local_evidence_fingerprint: read_array_32(bytes, &mut cursor)?,
        outcome_occurred_at_unix_ms: read_u64(bytes, &mut cursor)?,
        r3_intent_fingerprint: read_array_32(bytes, &mut cursor)?,
        r3_evidence_fingerprint: read_array_32(bytes, &mut cursor)?,
        dependency_kind: read_u8(bytes, &mut cursor)?,
    };
    if cursor != bytes.len() || encode_bridge_consumption(&witness) != bytes {
        return Err(Error::LocalExecutionJournalMalformed);
    }
    Ok(witness)
}

fn decode_common(bytes: &[u8], expected_kind: u8) -> Result<(PreSideEffectWitness, usize)> {
    let mut cursor = 0;
    if read_exact(bytes, &mut cursor, MAGIC.len())? != MAGIC
        || read_u8(bytes, &mut cursor)? != VERSION
        || read_u8(bytes, &mut cursor)? != expected_kind
    {
        return Err(Error::LocalExecutionJournalMalformed);
    }
    let witness = PreSideEffectWitness {
        operation_id: read_uuid(bytes, &mut cursor)?,
        attempt_number: read_u32(bytes, &mut cursor)?,
        boundary_id: read_uuid(bytes, &mut cursor)?,
        boundary_occurred_at_unix_ms: read_u64(bytes, &mut cursor)?,
        intent_fingerprint: read_array_32(bytes, &mut cursor)?,
        contract_fingerprint: read_array_32(bytes, &mut cursor)?,
        collision_snapshot_fingerprint: read_array_32(bytes, &mut cursor)?,
        created_at_unix_ms: read_u64(bytes, &mut cursor)?,
    };
    Ok((witness, cursor))
}

fn read_uuid(bytes: &[u8], cursor: &mut usize) -> Result<Uuid> {
    let value = Uuid::from_slice(read_exact(bytes, cursor, 16)?)
        .map_err(|_| Error::LocalExecutionJournalMalformed)?;
    if value.is_nil() {
        return Err(Error::LocalExecutionJournalMalformed);
    }
    Ok(value)
}

fn read_array_32(bytes: &[u8], cursor: &mut usize) -> Result<[u8; 32]> {
    read_exact(bytes, cursor, 32)?
        .try_into()
        .map_err(|_| Error::LocalExecutionJournalMalformed)
}

fn read_u8(bytes: &[u8], cursor: &mut usize) -> Result<u8> {
    Ok(read_exact(bytes, cursor, 1)?[0])
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        read_exact(bytes, cursor, 4)?
            .try_into()
            .map_err(|_| Error::LocalExecutionJournalMalformed)?,
    ))
}

fn read_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
    Ok(u64::from_be_bytes(
        read_exact(bytes, cursor, 8)?
            .try_into()
            .map_err(|_| Error::LocalExecutionJournalMalformed)?,
    ))
}

fn read_exact<'a>(bytes: &'a [u8], cursor: &mut usize, length: usize) -> Result<&'a [u8]> {
    let end = cursor
        .checked_add(length)
        .ok_or(Error::LocalExecutionJournalMalformed)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or(Error::LocalExecutionJournalMalformed)?;
    *cursor = end;
    Ok(value)
}

fn encode_outcome_kind(outcome: LocalExecutionOutcome) -> u8 {
    match outcome {
        LocalExecutionOutcome::VerifiedApplied => 1,
        LocalExecutionOutcome::VerifiedNotApplied => 2,
        LocalExecutionOutcome::WriteOutcomeUnknown => 3,
        LocalExecutionOutcome::NeedsReconcile => 4,
    }
}

fn decode_outcome_kind(value: u8) -> Result<LocalExecutionOutcome> {
    match value {
        1 => Ok(LocalExecutionOutcome::VerifiedApplied),
        2 => Ok(LocalExecutionOutcome::VerifiedNotApplied),
        3 => Ok(LocalExecutionOutcome::WriteOutcomeUnknown),
        4 => Ok(LocalExecutionOutcome::NeedsReconcile),
        _ => Err(Error::LocalExecutionJournalMalformed),
    }
}

#[cfg(unix)]
fn atomic_rename_noreplace(directory: &Dir, source: &str, destination: &str) -> Result<()> {
    let held = directory.try_clone()?.into_std_file();
    rustix::fs::renameat_with(
        &held,
        source,
        &held,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| Error::Io(io::Error::from(error)))
}

#[cfg(windows)]
fn atomic_rename_noreplace(directory: &Dir, source: &str, destination: &str) -> Result<()> {
    myvault_platform_fs::rename_noreplace(
        directory,
        OsStr::new(source),
        directory,
        OsStr::new(destination),
    )
    .map_err(Error::Io)
}

#[cfg(not(any(unix, windows)))]
fn atomic_rename_noreplace(_directory: &Dir, _source: &str, _destination: &str) -> Result<()> {
    Err(Error::LocalExecutionJournalMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pre() -> PreSideEffectWitness {
        PreSideEffectWitness {
            operation_id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
            attempt_number: 7,
            boundary_id: Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222),
            boundary_occurred_at_unix_ms: 11,
            intent_fingerprint: [1; 32],
            contract_fingerprint: [2; 32],
            collision_snapshot_fingerprint: [3; 32],
            created_at_unix_ms: 99,
        }
    }

    #[test]
    fn fixed_binary_witnesses_are_canonical_and_round_trip() {
        let pre = pre();
        let pre_bytes = encode_pre(&pre);
        assert_eq!(decode_pre(&pre_bytes).expect("decode pre"), pre);
        let outcome = OutcomeWitness {
            pre,
            outcome_id: Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333),
            evidence_id: Uuid::from_u128(0x4444_4444_4444_4444_4444_4444_4444_4444),
            outcome: LocalExecutionOutcome::VerifiedApplied,
            evidence_fingerprint: [4; 32],
            r3_mutation_evidence_fingerprint: Some([5; 32]),
            created_at_unix_ms: 101,
        };
        let outcome_bytes = encode_outcome(&outcome);
        assert_eq!(outcome_bytes.len(), 262);
        assert_eq!(outcome_bytes[221], 1, "fixed presence byte");
        assert_eq!(&outcome_bytes[222..254], &[5; 32]);
        assert_eq!(
            decode_outcome(&outcome_bytes).expect("decode outcome"),
            outcome
        );
        let unbridged = OutcomeWitness {
            r3_mutation_evidence_fingerprint: None,
            ..outcome
        };
        let unbridged_bytes = encode_outcome(&unbridged);
        assert_eq!(unbridged_bytes.len(), 230);
        assert_eq!(unbridged_bytes[221], 0, "fixed absence byte");
        assert_eq!(
            decode_outcome(&unbridged_bytes).expect("decode unbridged outcome"),
            unbridged
        );
        assert_ne!(
            outcome_bytes, unbridged_bytes,
            "presence is hash-bound bytes"
        );
    }

    #[test]
    fn truncated_and_unsupported_witnesses_fail_closed() {
        let pre = pre();
        let bytes = encode_pre(&pre);
        for length in 0..bytes.len() {
            assert!(decode_pre(&bytes[..length]).is_err(), "length {length}");
        }
        let mut unsupported = bytes;
        unsupported[MAGIC.len()] = VERSION + 1;
        assert!(decode_pre(&unsupported).is_err());
    }
}
