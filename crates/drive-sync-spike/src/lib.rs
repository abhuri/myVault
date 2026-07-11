//! Safety-critical state machines for the Phase 0 Google Drive spike.
//!
//! This crate deliberately contains no credentials or UI bindings. Network
//! adapters must feed verified outcomes into these types.

use std::collections::BTreeSet;
use thiserror::Error;

pub mod rest;

pub const FIXTURE_PREFIX: &str = "myVault-spike-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    ReauthorizeOnce,
    StopForbidden,
    RetryAfter,
    RetryWithBackoff,
    DoNotRetry,
}

pub fn classify_http_status(status: u16) -> RetryDecision {
    match status {
        401 => RetryDecision::ReauthorizeOnce,
        403 => RetryDecision::StopForbidden,
        429 => RetryDecision::RetryAfter,
        500..=599 => RetryDecision::RetryWithBackoff,
        _ => RetryDecision::DoNotRetry,
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SyncError {
    #[error("initial scan must capture a start token first")]
    MissingStartToken,
    #[error("initial scan has not completed")]
    ScanIncomplete,
    #[error("a change batch is already active")]
    BatchAlreadyActive,
    #[error("no change batch is active")]
    NoActiveBatch,
    #[error("not all local mutations committed")]
    LocalMutationIncomplete,
    #[error("mutation id was not declared in the active batch")]
    UnknownMutation,
    #[error("fixture folder is outside the Phase 0 allowlist")]
    FixtureNotAllowlisted,
    #[error("the live Drive fixture harness is disabled")]
    HarnessDisabled,
    #[error("Google Drive request failed: {0}")]
    Http(String),
    #[error("Google Drive returned HTTP {status} (response body redacted)")]
    HttpStatus { status: u16 },
    #[error("Google Drive returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error("download hash mismatch (expected {expected}, got {actual})")]
    HashMismatch { expected: String, actual: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitialSyncPhase {
    NeedStartToken,
    Scanning { start_token: String },
    Draining { start_token: String },
    Ready,
}

#[derive(Debug, Clone)]
pub struct InitialSync {
    phase: InitialSyncPhase,
}

impl Default for InitialSync {
    fn default() -> Self {
        Self {
            phase: InitialSyncPhase::NeedStartToken,
        }
    }
}

impl InitialSync {
    pub fn phase(&self) -> &InitialSyncPhase {
        &self.phase
    }

    pub fn capture_start_token(&mut self, token: impl Into<String>) {
        self.phase = InitialSyncPhase::Scanning {
            start_token: token.into(),
        };
    }

    pub fn finish_scan(&mut self) -> Result<(), SyncError> {
        let InitialSyncPhase::Scanning { start_token } = &self.phase else {
            return Err(SyncError::MissingStartToken);
        };
        self.phase = InitialSyncPhase::Draining {
            start_token: start_token.clone(),
        };
        Ok(())
    }

    pub fn finish_drain(&mut self) -> Result<(), SyncError> {
        if !matches!(self.phase, InitialSyncPhase::Draining { .. }) {
            return Err(SyncError::ScanIncomplete);
        }
        self.phase = InitialSyncPhase::Ready;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct PendingBatch {
    next_cursor: String,
    expected_mutations: BTreeSet<String>,
    committed_mutations: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DurableCursor {
    committed: Option<String>,
    pending: Option<PendingBatch>,
}

impl DurableCursor {
    pub fn committed(&self) -> Option<&str> {
        self.committed.as_deref()
    }

    pub fn begin_batch<I, S>(
        &mut self,
        next_cursor: impl Into<String>,
        expected_mutations: I,
    ) -> Result<(), SyncError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if self.pending.is_some() {
            return Err(SyncError::BatchAlreadyActive);
        }
        self.pending = Some(PendingBatch {
            next_cursor: next_cursor.into(),
            expected_mutations: expected_mutations.into_iter().map(Into::into).collect(),
            committed_mutations: BTreeSet::new(),
        });
        Ok(())
    }

    pub fn mark_local_commit(&mut self, mutation_id: impl Into<String>) -> Result<(), SyncError> {
        let pending = self.pending.as_mut().ok_or(SyncError::NoActiveBatch)?;
        let mutation_id = mutation_id.into();
        if !pending.expected_mutations.contains(&mutation_id) {
            return Err(SyncError::UnknownMutation);
        }
        pending.committed_mutations.insert(mutation_id);
        Ok(())
    }

    pub fn commit_cursor(&mut self) -> Result<(), SyncError> {
        let pending = self.pending.as_ref().ok_or(SyncError::NoActiveBatch)?;
        if pending.committed_mutations != pending.expected_mutations {
            return Err(SyncError::LocalMutationIncomplete);
        }
        let next_cursor = pending.next_cursor.clone();
        self.committed = Some(next_cursor);
        self.pending = None;
        Ok(())
    }

    pub fn abort_batch(&mut self) {
        self.pending = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCandidate {
    pub file_id: String,
    pub parent_id: String,
    pub name: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownUploadResolution {
    ConfirmExisting { file_id: String },
    RetryUpload,
    Conflict { candidate_ids: Vec<String> },
}

pub fn resolve_unknown_upload(
    intended_parent: &str,
    intended_name: &str,
    intended_hash: &str,
    candidates: &[RemoteCandidate],
) -> UnknownUploadResolution {
    let mut exact = candidates
        .iter()
        .filter(|candidate| {
            candidate.parent_id == intended_parent
                && candidate.name == intended_name
                && candidate.content_hash == intended_hash
        })
        .map(|candidate| candidate.file_id.clone())
        .collect::<Vec<_>>();
    exact.sort();

    match exact.as_slice() {
        [] => UnknownUploadResolution::RetryUpload,
        [file_id] => UnknownUploadResolution::ConfirmExisting {
            file_id: file_id.clone(),
        },
        _ => UnknownUploadResolution::Conflict {
            candidate_ids: exact,
        },
    }
}

pub fn verify_fixture_cleanup(folder_id: &str, folder_name: &str) -> Result<(), SyncError> {
    let valid_id = !folder_id.trim().is_empty()
        && folder_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    let suffix = folder_name.strip_prefix(FIXTURE_PREFIX);
    let valid_name = suffix.is_some_and(|value| {
        value.len() >= 12
            && value.chars().any(|character| character.is_ascii_digit())
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
    });

    if valid_id && valid_name {
        Ok(())
    } else {
        Err(SyncError::FixtureNotAllowlisted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_sync_captures_token_before_scan_then_drains() {
        let mut sync = InitialSync::default();
        assert_eq!(sync.finish_scan(), Err(SyncError::MissingStartToken));
        sync.capture_start_token("token-before-scan");
        sync.finish_scan().unwrap();
        assert!(matches!(sync.phase(), InitialSyncPhase::Draining { .. }));
        sync.finish_drain().unwrap();
        assert_eq!(sync.phase(), &InitialSyncPhase::Ready);
    }

    #[test]
    fn cursor_never_advances_before_all_local_commits() {
        let mut cursor = DurableCursor::default();
        cursor
            .begin_batch("page-2", ["write-thai-note", "write-attachment"])
            .unwrap();
        cursor.mark_local_commit("write-thai-note").unwrap();
        assert_eq!(
            cursor.commit_cursor(),
            Err(SyncError::LocalMutationIncomplete)
        );
        assert_eq!(cursor.committed(), None);
        cursor.mark_local_commit("write-attachment").unwrap();
        cursor.commit_cursor().unwrap();
        assert_eq!(cursor.committed(), Some("page-2"));
    }

    #[test]
    fn aborted_batch_keeps_previous_cursor() {
        let mut cursor = DurableCursor::default();
        cursor
            .begin_batch("page-1", std::iter::empty::<&str>())
            .unwrap();
        cursor.commit_cursor().unwrap();
        cursor.begin_batch("page-2", ["pending-write"]).unwrap();
        cursor.abort_batch();
        assert_eq!(cursor.committed(), Some("page-1"));
    }

    #[test]
    fn cursor_rejects_undeclared_mutation_even_when_counts_would_match() {
        let mut cursor = DurableCursor::default();
        cursor.begin_batch("page-2", ["expected-id"]).unwrap();
        assert_eq!(
            cursor.mark_local_commit("different-id"),
            Err(SyncError::UnknownMutation)
        );
        assert_eq!(
            cursor.commit_cursor(),
            Err(SyncError::LocalMutationIncomplete)
        );
        assert_eq!(cursor.committed(), None);
    }

    #[test]
    fn retry_matrix_is_explicit() {
        assert_eq!(classify_http_status(401), RetryDecision::ReauthorizeOnce);
        assert_eq!(classify_http_status(403), RetryDecision::StopForbidden);
        assert_eq!(classify_http_status(429), RetryDecision::RetryAfter);
        assert_eq!(classify_http_status(503), RetryDecision::RetryWithBackoff);
        assert_eq!(classify_http_status(400), RetryDecision::DoNotRetry);
    }

    #[test]
    fn unknown_upload_is_verified_before_retry() {
        let candidates = vec![RemoteCandidate {
            file_id: "drive-file-1".into(),
            parent_id: "fixture-parent".into(),
            name: "thai-สวัสดี.md".into(),
            content_hash: "sha256:abc".into(),
        }];
        assert_eq!(
            resolve_unknown_upload("fixture-parent", "thai-สวัสดี.md", "sha256:abc", &candidates),
            UnknownUploadResolution::ConfirmExisting {
                file_id: "drive-file-1".into()
            }
        );
    }

    #[test]
    fn cleanup_requires_exact_fixture_prefix_and_safe_id() {
        assert!(verify_fixture_cleanup("abc_DEF-123", "myVault-spike-20260711-a1b2c3").is_ok());
        assert_eq!(
            verify_fixture_cleanup("root", "Personal Vault"),
            Err(SyncError::FixtureNotAllowlisted)
        );
        assert_eq!(
            verify_fixture_cleanup("../../root", "myVault-spike-20260711-a1b2c3"),
            Err(SyncError::FixtureNotAllowlisted)
        );
    }
}
