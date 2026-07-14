#![forbid(unsafe_code)]

//! Tauri-free orchestration for guarded content transfers.
//!
//! This crate deliberately owns no OAuth token, provider response body, local
//! path capability, or content body. Platform adapters execute streaming I/O;
//! the worker validates their typed evidence and drives durable state through a
//! narrow store trait.

use myvault_sync_engine::{
    SyncStore, TransferCompletion, TransferDirection as StoreDirection, TransferMimeClass,
};
use std::{fmt, time::Duration};
use uuid::Uuid;

pub const SHA256_HEX_LENGTH: usize = 64;
pub const MAX_REDACTED_CODE_BYTES: usize = 96;
pub const MAX_PORTABLE_PATH_BYTES: usize = 4 * 1024;
pub const MAX_REMOTE_ID_BYTES: usize = 4 * 1024;
pub const MAX_TRANSFER_BYTES: u64 = 512 * 1024 * 1024;
pub const BASE_BACKOFF_MS: u64 = 1_000;
pub const MAX_BACKOFF_MS: u64 = 15 * 60 * 1_000;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferDirection {
    Upload,
    Download,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentKind {
    Markdown,
    Blob,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferIntent {
    operation_id: Uuid,
    direction: TransferDirection,
    path: String,
    parent_id: String,
    remote_file_id: Option<String>,
    expected_local_revision: Option<String>,
    expected_remote_revision: Option<String>,
    sha256_hex: String,
    byte_len: u64,
    content_kind: ContentKind,
    operation_marker: String,
    stage_ref: Option<String>,
    base_ref: Option<String>,
    attempt_count: u32,
}

impl TransferIntent {
    /// Builds one validated durable transfer intent without content or secrets.
    ///
    /// # Errors
    /// Returns [`Error::InvalidIntent`] or [`Error::InvalidEvidence`] when an
    /// identifier, path, digest, revision, marker, or size violates the R2
    /// transfer contract.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation_id: Uuid,
        direction: TransferDirection,
        path: impl Into<String>,
        parent_id: impl Into<String>,
        remote_file_id: Option<String>,
        expected_local_revision: Option<String>,
        expected_remote_revision: Option<String>,
        sha256_hex: impl Into<String>,
        byte_len: u64,
        content_kind: ContentKind,
        operation_marker: impl Into<String>,
        stage_ref: Option<String>,
        base_ref: Option<String>,
        attempt_count: u32,
    ) -> Result<Self> {
        let value = Self {
            operation_id,
            direction,
            path: path.into(),
            parent_id: parent_id.into(),
            remote_file_id,
            expected_local_revision,
            expected_remote_revision,
            sha256_hex: sha256_hex.into(),
            byte_len,
            content_kind,
            operation_marker: operation_marker.into(),
            stage_ref,
            base_ref,
            attempt_count,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<()> {
        if self.operation_id.is_nil() {
            return Err(Error::InvalidIntent);
        }
        validate_portable_path(&self.path)?;
        validate_opaque(&self.parent_id, MAX_REMOTE_ID_BYTES)?;
        if let Some(file_id) = self.remote_file_id.as_deref() {
            validate_opaque(file_id, MAX_REMOTE_ID_BYTES)?;
        }
        if let Some(revision) = self.expected_local_revision.as_deref() {
            validate_opaque(revision, MAX_REMOTE_ID_BYTES)?;
        }
        if let Some(revision) = self.expected_remote_revision.as_deref() {
            validate_opaque(revision, MAX_REMOTE_ID_BYTES)?;
        }
        validate_sha256(&self.sha256_hex)?;
        if self.byte_len > MAX_TRANSFER_BYTES {
            return Err(Error::InvalidIntent);
        }
        validate_opaque(&self.operation_marker, MAX_REMOTE_ID_BYTES)?;
        if let Some(stage_ref) = self.stage_ref.as_deref() {
            validate_opaque(stage_ref, MAX_REMOTE_ID_BYTES)?;
        }
        if let Some(base_ref) = self.base_ref.as_deref() {
            validate_opaque(base_ref, MAX_REMOTE_ID_BYTES)?;
        }
        if self.direction == TransferDirection::Download && self.remote_file_id.is_none() {
            return Err(Error::InvalidIntent);
        }
        if self.direction == TransferDirection::Upload
            && (self.expected_local_revision.is_none() || self.stage_ref.is_none())
        {
            return Err(Error::InvalidIntent);
        }
        Ok(())
    }

    #[must_use]
    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    #[must_use]
    pub const fn direction(&self) -> TransferDirection {
        self.direction
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn parent_id(&self) -> &str {
        &self.parent_id
    }

    #[must_use]
    pub fn remote_file_id(&self) -> Option<&str> {
        self.remote_file_id.as_deref()
    }

    #[must_use]
    pub fn expected_local_revision(&self) -> Option<&str> {
        self.expected_local_revision.as_deref()
    }

    #[must_use]
    pub fn expected_remote_revision(&self) -> Option<&str> {
        self.expected_remote_revision.as_deref()
    }

    #[must_use]
    pub fn sha256_hex(&self) -> &str {
        &self.sha256_hex
    }

    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    #[must_use]
    pub const fn content_kind(&self) -> ContentKind {
        self.content_kind
    }

    #[must_use]
    pub fn operation_marker(&self) -> &str {
        &self.operation_marker
    }

    #[must_use]
    pub fn stage_ref(&self) -> Option<&str> {
        self.stage_ref.as_deref()
    }

    #[must_use]
    pub fn base_ref(&self) -> Option<&str> {
        self.base_ref.as_deref()
    }

    #[must_use]
    pub const fn attempt_count(&self) -> u32 {
        self.attempt_count
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedTransfer {
    operation_id: Uuid,
    remote_file_id: String,
    remote_revision: String,
    local_revision: Option<String>,
    sha256_hex: String,
    byte_len: u64,
    base_ref: String,
    outcome_code: &'static str,
}

impl VerifiedTransfer {
    /// Builds typed evidence for one byte-verified transfer completion.
    ///
    /// # Errors
    /// Returns [`Error::InvalidEvidence`] when the evidence is malformed,
    /// oversized, secret-shaped, or incomplete.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation_id: Uuid,
        remote_file_id: impl Into<String>,
        remote_revision: impl Into<String>,
        local_revision: Option<String>,
        sha256_hex: impl Into<String>,
        byte_len: u64,
        base_ref: impl Into<String>,
        outcome_code: &'static str,
    ) -> Result<Self> {
        let value = Self {
            operation_id,
            remote_file_id: remote_file_id.into(),
            remote_revision: remote_revision.into(),
            local_revision,
            sha256_hex: sha256_hex.into(),
            byte_len,
            base_ref: base_ref.into(),
            outcome_code,
        };
        validate_opaque(&value.remote_file_id, MAX_REMOTE_ID_BYTES)?;
        validate_opaque(&value.remote_revision, MAX_REMOTE_ID_BYTES)?;
        if let Some(revision) = value.local_revision.as_deref() {
            validate_opaque(revision, MAX_REMOTE_ID_BYTES)?;
        }
        validate_sha256(&value.sha256_hex)?;
        validate_opaque(&value.base_ref, MAX_REMOTE_ID_BYTES)?;
        validate_redacted_code(value.outcome_code)?;
        if value.operation_id.is_nil() || value.byte_len > MAX_TRANSFER_BYTES {
            return Err(Error::InvalidEvidence);
        }
        Ok(value)
    }

    fn matches(&self, intent: &TransferIntent) -> bool {
        self.operation_id == intent.operation_id
            && self.sha256_hex == intent.sha256_hex
            && self.byte_len == intent.byte_len
    }

    #[must_use]
    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    #[must_use]
    pub fn remote_file_id(&self) -> &str {
        &self.remote_file_id
    }

    #[must_use]
    pub fn remote_revision(&self) -> &str {
        &self.remote_revision
    }

    #[must_use]
    pub fn local_revision(&self) -> Option<&str> {
        self.local_revision.as_deref()
    }

    #[must_use]
    pub fn sha256_hex(&self) -> &str {
        &self.sha256_hex
    }

    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    #[must_use]
    pub fn base_ref(&self) -> &str {
        &self.base_ref
    }

    #[must_use]
    pub const fn outcome_code(&self) -> &'static str {
        self.outcome_code
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionFailureKind {
    AuthRequired,
    Offline,
    RateLimited,
    /// A transient failure proven to have happened before any externally
    /// visible side effect. Retrying the exact durable intent is safe.
    TransientSafe,
    TransientUnknown,
    Permanent,
    NeedsReconcile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionFailure {
    kind: ExecutionFailureKind,
    code: &'static str,
    retry_after: Option<Duration>,
}

impl ExecutionFailure {
    /// Builds one redacted execution failure classification.
    ///
    /// # Errors
    /// Returns [`Error::InvalidEvidence`] for a malformed code or when retry
    /// timing is attached to a non-rate-limit classification.
    pub fn new(
        kind: ExecutionFailureKind,
        code: &'static str,
        retry_after: Option<Duration>,
    ) -> Result<Self> {
        validate_redacted_code(code)?;
        if kind != ExecutionFailureKind::RateLimited && retry_after.is_some() {
            return Err(Error::InvalidEvidence);
        }
        Ok(Self {
            kind,
            code,
            retry_after,
        })
    }

    #[must_use]
    pub const fn kind(&self) -> ExecutionFailureKind {
        self.kind
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

#[allow(clippy::missing_errors_doc)]
pub trait TransferStore {
    fn claim_due(&mut self, now_unix_ms: u64) -> Result<Option<TransferIntent>>;
    fn begin_local_publish(&mut self, operation_id: Uuid, now_unix_ms: u64) -> Result<()>;
    fn complete_verified(&mut self, verified: &VerifiedTransfer, now_unix_ms: u64) -> Result<()>;
    fn schedule_retry(
        &mut self,
        operation_id: Uuid,
        next_attempt_at_unix_ms: u64,
        code: &'static str,
    ) -> Result<()>;
    fn pause_offline(
        &mut self,
        operation_id: Uuid,
        next_attempt_at_unix_ms: u64,
        code: &'static str,
    ) -> Result<()>;
    fn mark_auth_required(
        &mut self,
        operation_id: Uuid,
        code: &'static str,
        now_unix_ms: u64,
    ) -> Result<()>;
    fn mark_needs_reconcile(
        &mut self,
        operation_id: Uuid,
        code: &'static str,
        now_unix_ms: u64,
    ) -> Result<()>;
}

impl<T> TransferStore for &mut T
where
    T: TransferStore + ?Sized,
{
    fn claim_due(&mut self, now_unix_ms: u64) -> Result<Option<TransferIntent>> {
        (**self).claim_due(now_unix_ms)
    }

    fn begin_local_publish(&mut self, operation_id: Uuid, now_unix_ms: u64) -> Result<()> {
        (**self).begin_local_publish(operation_id, now_unix_ms)
    }

    fn complete_verified(&mut self, verified: &VerifiedTransfer, now_unix_ms: u64) -> Result<()> {
        (**self).complete_verified(verified, now_unix_ms)
    }

    fn schedule_retry(
        &mut self,
        operation_id: Uuid,
        next_attempt_at_unix_ms: u64,
        code: &'static str,
    ) -> Result<()> {
        (**self).schedule_retry(operation_id, next_attempt_at_unix_ms, code)
    }

    fn pause_offline(
        &mut self,
        operation_id: Uuid,
        next_attempt_at_unix_ms: u64,
        code: &'static str,
    ) -> Result<()> {
        (**self).pause_offline(operation_id, next_attempt_at_unix_ms, code)
    }

    fn mark_auth_required(
        &mut self,
        operation_id: Uuid,
        code: &'static str,
        now_unix_ms: u64,
    ) -> Result<()> {
        (**self).mark_auth_required(operation_id, code, now_unix_ms)
    }

    fn mark_needs_reconcile(
        &mut self,
        operation_id: Uuid,
        code: &'static str,
        now_unix_ms: u64,
    ) -> Result<()> {
        (**self).mark_needs_reconcile(operation_id, code, now_unix_ms)
    }
}

impl TransferStore for SyncStore {
    fn claim_due(&mut self, now_unix_ms: u64) -> Result<Option<TransferIntent>> {
        let Some(record) = self
            .claim_next_transfer(now_unix_ms)
            .map_err(|_| Error::Store)?
        else {
            return Ok(None);
        };
        TransferIntent::new(
            record.operation_id,
            match record.direction {
                StoreDirection::Upload => TransferDirection::Upload,
                StoreDirection::Download => TransferDirection::Download,
            },
            record.portable_path,
            record.remote_parent_id,
            record.remote_file_id,
            record.expected_local_revision,
            record.expected_remote_revision,
            record.sha256,
            record.byte_length,
            match record.mime_class {
                TransferMimeClass::Markdown => ContentKind::Markdown,
                TransferMimeClass::Blob => ContentKind::Blob,
            },
            record.operation_marker,
            record.stage_reference,
            record.base_reference,
            record.attempt_count,
        )
        .map(Some)
    }

    fn begin_local_publish(&mut self, operation_id: Uuid, now_unix_ms: u64) -> Result<()> {
        let Some(batch) = self.active_change_batch().map_err(|_| Error::Store)? else {
            return Ok(());
        };
        let mutation_id = operation_id.to_string();
        if !self
            .local_mutations(batch.batch_id)
            .map_err(|_| Error::Store)?
            .iter()
            .any(|mutation| mutation.mutation_id == mutation_id)
        {
            return Ok(());
        }
        self.begin_transfer_local_publish(operation_id, now_unix_ms)
            .map_err(|_| Error::Store)
    }

    fn complete_verified(&mut self, verified: &VerifiedTransfer, now_unix_ms: u64) -> Result<()> {
        let local_revision = verified.local_revision().ok_or(Error::InvalidEvidence)?;
        let completion = TransferCompletion::new(
            verified.remote_file_id(),
            verified.remote_revision(),
            local_revision,
            verified.base_ref(),
            verified.outcome_code(),
            now_unix_ms,
        )
        .map_err(|_| Error::InvalidEvidence)?;
        self.complete_verified_transfer(verified.operation_id(), &completion)
            .map(|_| ())
            .map_err(|_| Error::Store)
    }

    fn schedule_retry(
        &mut self,
        operation_id: Uuid,
        next_attempt_at_unix_ms: u64,
        code: &'static str,
    ) -> Result<()> {
        self.schedule_transfer_retry(
            operation_id,
            next_attempt_at_unix_ms,
            code,
            next_attempt_at_unix_ms,
        )
        .map_err(|_| Error::Store)
    }

    fn pause_offline(
        &mut self,
        operation_id: Uuid,
        next_attempt_at_unix_ms: u64,
        code: &'static str,
    ) -> Result<()> {
        self.pause_transfer_offline(
            operation_id,
            next_attempt_at_unix_ms,
            code,
            next_attempt_at_unix_ms,
        )
        .map_err(|_| Error::Store)
    }

    fn mark_auth_required(
        &mut self,
        operation_id: Uuid,
        code: &'static str,
        now_unix_ms: u64,
    ) -> Result<()> {
        self.mark_transfer_auth_required(operation_id, code, now_unix_ms)
            .map_err(|_| Error::Store)
    }

    fn mark_needs_reconcile(
        &mut self,
        operation_id: Uuid,
        code: &'static str,
        now_unix_ms: u64,
    ) -> Result<()> {
        self.mark_transfer_needs_reconcile(operation_id, code, now_unix_ms)
            .map_err(|_| Error::Store)
    }
}

#[allow(clippy::missing_errors_doc)]
pub trait TransferExecutor {
    fn execute(
        &mut self,
        intent: &TransferIntent,
        before_local_publish: &mut dyn FnMut() -> Result<()>,
    ) -> std::result::Result<VerifiedTransfer, ExecutionFailure>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkOutcome {
    Idle,
    Completed(Uuid),
    RetryScheduled(Uuid),
    AuthRequired(Uuid),
    NeedsReconcile(Uuid),
}

pub struct Worker<S, E> {
    store: S,
    executor: E,
}

impl<S, E> Worker<S, E>
where
    S: TransferStore,
    E: TransferExecutor,
{
    #[must_use]
    pub const fn new(store: S, executor: E) -> Self {
        Self { store, executor }
    }

    /// Claims and advances at most one durable transfer operation.
    ///
    /// # Errors
    /// Returns a store or timestamp error without discarding the claimed
    /// operation's durable evidence.
    pub fn run_once(&mut self, now_unix_ms: u64) -> Result<WorkOutcome> {
        let Some(intent) = self.store.claim_due(now_unix_ms)? else {
            return Ok(WorkOutcome::Idle);
        };
        let operation_id = intent.operation_id();
        let mut gate_error = None;
        let execution = {
            let (store, executor) = (&mut self.store, &mut self.executor);
            let mut before_local_publish = || {
                let result = store.begin_local_publish(operation_id, now_unix_ms);
                if let Err(error) = result {
                    gate_error = Some(error);
                }
                result
            };
            executor.execute(&intent, &mut before_local_publish)
        };
        if let Some(error) = gate_error {
            return Err(error);
        }
        match execution {
            Ok(verified) => {
                if !verified.matches(&intent) {
                    self.store.mark_needs_reconcile(
                        operation_id,
                        "verified_evidence_mismatch",
                        now_unix_ms,
                    )?;
                    return Ok(WorkOutcome::NeedsReconcile(operation_id));
                }
                self.store.complete_verified(&verified, now_unix_ms)?;
                Ok(WorkOutcome::Completed(operation_id))
            }
            Err(failure) => {
                self.handle_failure(operation_id, now_unix_ms, intent.attempt_count(), &failure)
            }
        }
    }

    fn handle_failure(
        &mut self,
        operation_id: Uuid,
        now_unix_ms: u64,
        attempt: u32,
        failure: &ExecutionFailure,
    ) -> Result<WorkOutcome> {
        match failure.kind() {
            ExecutionFailureKind::AuthRequired => {
                self.store
                    .mark_auth_required(operation_id, failure.code(), now_unix_ms)?;
                Ok(WorkOutcome::AuthRequired(operation_id))
            }
            ExecutionFailureKind::Offline => {
                let next = checked_add_ms(now_unix_ms, BASE_BACKOFF_MS)?;
                self.store
                    .pause_offline(operation_id, next, failure.code())?;
                Ok(WorkOutcome::RetryScheduled(operation_id))
            }
            ExecutionFailureKind::RateLimited => {
                let requested = failure
                    .retry_after()
                    .and_then(|value| u64::try_from(value.as_millis()).ok())
                    .unwrap_or_else(|| retry_delay_ms(operation_id, attempt));
                let delay = requested.clamp(BASE_BACKOFF_MS, MAX_BACKOFF_MS);
                let next = checked_add_ms(now_unix_ms, delay)?;
                self.store
                    .schedule_retry(operation_id, next, failure.code())?;
                Ok(WorkOutcome::RetryScheduled(operation_id))
            }
            ExecutionFailureKind::TransientSafe => {
                let delay = retry_delay_ms(operation_id, attempt);
                let next = checked_add_ms(now_unix_ms, delay)?;
                self.store
                    .schedule_retry(operation_id, next, failure.code())?;
                Ok(WorkOutcome::RetryScheduled(operation_id))
            }
            ExecutionFailureKind::Permanent
            | ExecutionFailureKind::TransientUnknown
            | ExecutionFailureKind::NeedsReconcile => {
                self.store
                    .mark_needs_reconcile(operation_id, failure.code(), now_unix_ms)?;
                Ok(WorkOutcome::NeedsReconcile(operation_id))
            }
        }
    }

    #[must_use]
    pub fn into_parts(self) -> (S, E) {
        (self.store, self.executor)
    }
}

#[must_use]
pub fn retry_delay_ms(operation_id: Uuid, attempt: u32) -> u64 {
    let shift = attempt.min(9);
    let exponential = BASE_BACKOFF_MS.saturating_mul(1_u64 << shift);
    let capped = exponential.min(MAX_BACKOFF_MS);
    let jitter_bound = (capped / 4).max(1);
    let bytes = operation_id.as_bytes();
    let seed = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    capped
        .saturating_add(seed % jitter_bound)
        .min(MAX_BACKOFF_MS)
}

fn checked_add_ms(now_unix_ms: u64, delay_ms: u64) -> Result<u64> {
    now_unix_ms
        .checked_add(delay_ms)
        .ok_or(Error::TimestampOverflow)
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != SHA256_HEX_LENGTH
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::InvalidEvidence);
    }
    Ok(())
}

fn validate_redacted_code(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_REDACTED_CODE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(Error::InvalidEvidence);
    }
    Ok(())
}

fn validate_opaque(value: &str, max_bytes: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
        || value.contains(['/', '\\'])
    {
        return Err(Error::InvalidEvidence);
    }
    Ok(())
}

fn validate_portable_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_PORTABLE_PATH_BYTES
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(Error::InvalidIntent);
    }
    let mut parts = value.split('/');
    if parts.next().is_some_and(|part| {
        part.eq_ignore_ascii_case(".obsidian") || part.eq_ignore_ascii_case(".trash")
    }) {
        return Err(Error::InvalidIntent);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidIntent,
    InvalidEvidence,
    Store,
    TimestampOverflow,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIntent => formatter.write_str("the transfer intent is invalid"),
            Self::InvalidEvidence => formatter.write_str("the transfer evidence is invalid"),
            Self::Store => formatter.write_str("the durable transfer store is unavailable"),
            Self::TimestampOverflow => formatter.write_str("the transfer timestamp is invalid"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn intent(operation_id: Uuid) -> TransferIntent {
        TransferIntent::new(
            operation_id,
            TransferDirection::Upload,
            "Notes/ภาษาไทย.md",
            "parent1",
            None,
            Some("localrev1".into()),
            None,
            HASH_A,
            12,
            ContentKind::Markdown,
            "operation1",
            Some("stage-operation1".into()),
            None,
            0,
        )
        .unwrap()
    }

    fn verified(operation_id: Uuid, hash: &str) -> VerifiedTransfer {
        VerifiedTransfer::new(
            operation_id,
            "remote1",
            "remoteversion1",
            Some("localrev1".into()),
            hash,
            12,
            "sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "upload_verified",
        )
        .unwrap()
    }

    #[derive(Default)]
    struct MemoryStore {
        jobs: VecDeque<TransferIntent>,
        local_publish: Vec<Uuid>,
        completed: Vec<Uuid>,
        retries: Vec<(Uuid, u64, &'static str)>,
        offline_pauses: Vec<(Uuid, u64, &'static str)>,
        auth: Vec<Uuid>,
        reconcile: Vec<Uuid>,
    }

    impl TransferStore for MemoryStore {
        fn claim_due(&mut self, _now_unix_ms: u64) -> Result<Option<TransferIntent>> {
            Ok(self.jobs.pop_front())
        }

        fn begin_local_publish(&mut self, operation_id: Uuid, _now_unix_ms: u64) -> Result<()> {
            self.local_publish.push(operation_id);
            Ok(())
        }

        fn complete_verified(
            &mut self,
            verified: &VerifiedTransfer,
            _now_unix_ms: u64,
        ) -> Result<()> {
            self.completed.push(verified.operation_id());
            Ok(())
        }

        fn schedule_retry(
            &mut self,
            operation_id: Uuid,
            next_attempt_at_unix_ms: u64,
            code: &'static str,
        ) -> Result<()> {
            self.retries
                .push((operation_id, next_attempt_at_unix_ms, code));
            Ok(())
        }

        fn pause_offline(
            &mut self,
            operation_id: Uuid,
            next_attempt_at_unix_ms: u64,
            code: &'static str,
        ) -> Result<()> {
            self.offline_pauses
                .push((operation_id, next_attempt_at_unix_ms, code));
            Ok(())
        }

        fn mark_auth_required(
            &mut self,
            operation_id: Uuid,
            _code: &'static str,
            _now_unix_ms: u64,
        ) -> Result<()> {
            self.auth.push(operation_id);
            Ok(())
        }

        fn mark_needs_reconcile(
            &mut self,
            operation_id: Uuid,
            _code: &'static str,
            _now_unix_ms: u64,
        ) -> Result<()> {
            self.reconcile.push(operation_id);
            Ok(())
        }
    }

    struct ScriptedExecutor {
        results: VecDeque<std::result::Result<VerifiedTransfer, ExecutionFailure>>,
    }

    impl TransferExecutor for ScriptedExecutor {
        fn execute(
            &mut self,
            _intent: &TransferIntent,
            _before_local_publish: &mut dyn FnMut() -> Result<()>,
        ) -> std::result::Result<VerifiedTransfer, ExecutionFailure> {
            self.results.pop_front().expect("scripted result")
        }
    }

    struct LocalPublishExecutor {
        verified: Option<VerifiedTransfer>,
    }

    impl TransferExecutor for LocalPublishExecutor {
        fn execute(
            &mut self,
            _intent: &TransferIntent,
            before_local_publish: &mut dyn FnMut() -> Result<()>,
        ) -> std::result::Result<VerifiedTransfer, ExecutionFailure> {
            before_local_publish().map_err(|_| {
                ExecutionFailure::new(
                    ExecutionFailureKind::NeedsReconcile,
                    "transfer_store_unavailable",
                    None,
                )
                .expect("failure")
            })?;
            Ok(self.verified.take().expect("verified result"))
        }
    }

    #[test]
    fn verified_completion_requires_exact_operation_hash_and_length() {
        let id = Uuid::new_v4();
        let store = MemoryStore {
            jobs: VecDeque::from([intent(id)]),
            ..MemoryStore::default()
        };
        let executor = ScriptedExecutor {
            results: VecDeque::from([Ok(verified(id, HASH_A))]),
        };
        let mut worker = Worker::new(store, executor);
        assert_eq!(worker.run_once(10).unwrap(), WorkOutcome::Completed(id));
        let (store, _) = worker.into_parts();
        assert_eq!(store.completed, [id]);
        assert!(store.reconcile.is_empty());
    }

    #[test]
    fn download_requests_durable_local_publish_gate_before_completion() {
        let id = Uuid::new_v4();
        let mut due = intent(id);
        due.direction = TransferDirection::Download;
        due.remote_file_id = Some("remote1".to_owned());
        due.expected_remote_revision = Some("remoteversion1".to_owned());
        due.expected_local_revision = None;
        let store = MemoryStore {
            jobs: VecDeque::from([due]),
            ..MemoryStore::default()
        };
        let executor = LocalPublishExecutor {
            verified: Some(verified(id, HASH_A)),
        };
        let mut worker = Worker::new(store, executor);

        assert_eq!(worker.run_once(10).unwrap(), WorkOutcome::Completed(id));

        let (store, _) = worker.into_parts();
        assert_eq!(store.local_publish, [id]);
        assert_eq!(store.completed, [id]);
    }

    #[test]
    fn mismatched_verified_evidence_needs_reconcile() {
        let id = Uuid::new_v4();
        let store = MemoryStore {
            jobs: VecDeque::from([intent(id)]),
            ..MemoryStore::default()
        };
        let executor = ScriptedExecutor {
            results: VecDeque::from([Ok(verified(id, HASH_B))]),
        };
        let mut worker = Worker::new(store, executor);
        assert_eq!(
            worker.run_once(10).unwrap(),
            WorkOutcome::NeedsReconcile(id)
        );
        let (store, _) = worker.into_parts();
        assert!(store.completed.is_empty());
        assert_eq!(store.reconcile, [id]);
    }

    #[test]
    fn unknown_outcome_never_schedules_blind_retry() {
        let id = Uuid::new_v4();
        let store = MemoryStore {
            jobs: VecDeque::from([intent(id)]),
            ..MemoryStore::default()
        };
        let failure = ExecutionFailure::new(
            ExecutionFailureKind::TransientUnknown,
            "remote_outcome_unknown",
            None,
        )
        .unwrap();
        let executor = ScriptedExecutor {
            results: VecDeque::from([Err(failure)]),
        };
        let mut worker = Worker::new(store, executor);
        assert_eq!(
            worker.run_once(10).unwrap(),
            WorkOutcome::NeedsReconcile(id)
        );
        let (store, _) = worker.into_parts();
        assert!(store.retries.is_empty());
        assert_eq!(store.reconcile, [id]);
    }

    #[test]
    fn rate_limit_honors_bounded_retry_after() {
        let id = Uuid::new_v4();
        let store = MemoryStore {
            jobs: VecDeque::from([intent(id)]),
            ..MemoryStore::default()
        };
        let failure = ExecutionFailure::new(
            ExecutionFailureKind::RateLimited,
            "drive_rate_limited",
            Some(Duration::from_secs(120)),
        )
        .unwrap();
        let executor = ScriptedExecutor {
            results: VecDeque::from([Err(failure)]),
        };
        let mut worker = Worker::new(store, executor);
        assert_eq!(
            worker.run_once(1_000).unwrap(),
            WorkOutcome::RetryScheduled(id)
        );
        let (store, _) = worker.into_parts();
        assert_eq!(store.retries, [(id, 121_000, "drive_rate_limited")]);
    }

    #[test]
    fn retry_backoff_uses_the_durable_attempt_count() {
        let id = Uuid::new_v4();
        let mut due = intent(id);
        due.attempt_count = 4;
        let store = MemoryStore {
            jobs: VecDeque::from([due]),
            ..MemoryStore::default()
        };
        let failure = ExecutionFailure::new(
            ExecutionFailureKind::RateLimited,
            "drive_rate_limited",
            None,
        )
        .unwrap();
        let executor = ScriptedExecutor {
            results: VecDeque::from([Err(failure)]),
        };
        let mut worker = Worker::new(store, executor);
        assert_eq!(
            worker.run_once(1_000).unwrap(),
            WorkOutcome::RetryScheduled(id)
        );
        let (store, _) = worker.into_parts();
        assert_eq!(
            store.retries,
            [(id, 1_000 + retry_delay_ms(id, 4), "drive_rate_limited")]
        );
    }

    #[test]
    fn proven_pre_side_effect_transient_failure_schedules_retry() {
        let id = Uuid::new_v4();
        let store = MemoryStore {
            jobs: VecDeque::from([intent(id)]),
            ..MemoryStore::default()
        };
        let failure = ExecutionFailure::new(
            ExecutionFailureKind::TransientSafe,
            "drive_transport_before_side_effect",
            None,
        )
        .unwrap();
        let executor = ScriptedExecutor {
            results: VecDeque::from([Err(failure)]),
        };
        let mut worker = Worker::new(store, executor);
        assert_eq!(
            worker.run_once(1_000).unwrap(),
            WorkOutcome::RetryScheduled(id)
        );
        let (store, _) = worker.into_parts();
        assert_eq!(
            store.retries,
            [(
                id,
                1_000 + retry_delay_ms(id, 0),
                "drive_transport_before_side_effect"
            )]
        );
        assert!(store.reconcile.is_empty());
    }

    #[test]
    fn auth_failure_stops_without_retry() {
        let id = Uuid::new_v4();
        let store = MemoryStore {
            jobs: VecDeque::from([intent(id)]),
            ..MemoryStore::default()
        };
        let failure = ExecutionFailure::new(
            ExecutionFailureKind::AuthRequired,
            "drive_auth_required",
            None,
        )
        .unwrap();
        let executor = ScriptedExecutor {
            results: VecDeque::from([Err(failure)]),
        };
        let mut worker = Worker::new(store, executor);
        assert_eq!(
            worker.run_once(1_000).unwrap(),
            WorkOutcome::AuthRequired(id)
        );
        let (store, _) = worker.into_parts();
        assert_eq!(store.auth, [id]);
        assert!(store.retries.is_empty());
    }

    #[test]
    fn offline_pause_does_not_consume_a_retry_attempt() {
        let id = Uuid::new_v4();
        let store = MemoryStore {
            jobs: VecDeque::from([intent(id)]),
            ..MemoryStore::default()
        };
        let failure =
            ExecutionFailure::new(ExecutionFailureKind::Offline, "network_offline", None).unwrap();
        let executor = ScriptedExecutor {
            results: VecDeque::from([Err(failure)]),
        };
        let mut worker = Worker::new(store, executor);
        assert_eq!(
            worker.run_once(2_000).unwrap(),
            WorkOutcome::RetryScheduled(id)
        );
        let (store, _) = worker.into_parts();
        assert_eq!(store.offline_pauses, [(id, 3_000, "network_offline")]);
        assert!(store.retries.is_empty());
    }

    #[test]
    fn path_and_identity_validation_is_fail_closed() {
        let id = Uuid::new_v4();
        for path in [
            "",
            "/absolute.md",
            "../escape.md",
            ".trash/item",
            ".Obsidian/a",
        ] {
            assert!(TransferIntent::new(
                id,
                TransferDirection::Upload,
                path,
                "parent",
                None,
                None,
                None,
                HASH_A,
                1,
                ContentKind::Blob,
                "marker",
                None,
                None,
                0,
            )
            .is_err());
        }
        assert!(TransferIntent::new(
            id,
            TransferDirection::Download,
            "safe.bin",
            "parent",
            None,
            None,
            Some("remote1".into()),
            HASH_A,
            1,
            ContentKind::Blob,
            "marker",
            None,
            None,
            0,
        )
        .is_err());
    }

    #[test]
    fn retry_delay_is_deterministic_bounded_and_nonzero() {
        let id = Uuid::from_u128(0x0102_0304_0506_0708_1112_1314_1516_1718);
        assert_eq!(retry_delay_ms(id, 3), retry_delay_ms(id, 3));
        assert!(retry_delay_ms(id, 0) >= BASE_BACKOFF_MS);
        assert!(retry_delay_ms(id, u32::MAX) <= MAX_BACKOFF_MS);
    }

    #[test]
    fn empty_queue_is_idle() {
        let executor = ScriptedExecutor {
            results: VecDeque::new(),
        };
        let mut worker = Worker::new(MemoryStore::default(), executor);
        assert_eq!(worker.run_once(5).unwrap(), WorkOutcome::Idle);
    }
}
