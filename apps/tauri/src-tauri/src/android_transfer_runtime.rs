#![forbid(unsafe_code)]

//! Host-testable Android transfer orchestration.
//!
//! Concrete SAF and private-root plugins deliberately do not appear here. An
//! adapter must bind each trait object to one exact vault capability before it
//! constructs this executor. Whole bodies never cross the frontend boundary;
//! the Android plugin adapter moves native bytes over a bounded, transcript-
//! checked chunk bridge before constructing these `Vec<u8>` values.

use crate::android_transfer_policy::{
    prepare_saf_transfer, AndroidTransferDirection, AndroidTransferPolicyError,
    ExpectedSafEvidence, ANDROID_MAX_TRANSFER_BYTES,
};
use myvault_core::FileRevision;
use myvault_drive::{
    plan_resumable_upload_chunk, CreateIntent, CreateReconciliation, DownloadIntent,
    Error as DriveError, ErrorCode as DriveErrorCode, RemoteObject, TransferDrive, UploadProgress,
};
use myvault_transfer::{
    ContentKind, ExecutionFailure, ExecutionFailureKind, TransferDirection, TransferExecutor,
    TransferIntent, VerifiedTransfer,
};
use std::time::Duration;
use uuid::Uuid;

/// Stable failures from an adapter already bound to one exact SAF capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AndroidVaultIoError {
    AlreadyExists,
    NotFound,
    InvalidRequest,
    ProtectedPath,
    ResourceLimit,
    VaultUnavailable,
    PublicationUnknown,
}

/// The only user-vault operations available to Android transfer orchestration.
///
/// Implementations must be constructed with one exact SAF capability. They
/// must reject ambient paths, replacement writes, and bodies above `max_bytes`.
pub(crate) trait AndroidVaultIo {
    fn read_exact(
        &mut self,
        portable_path: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, AndroidVaultIoError>;

    fn create_no_replace(
        &mut self,
        portable_path: &str,
        body: Vec<u8>,
        max_bytes: usize,
    ) -> Result<(), AndroidVaultIoError>;
}

/// Stable failures from a private transfer store bound to one vault identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AndroidPrivateStoreError {
    AlreadyExists,
    StageUnavailable,
    ResourceLimit,
    StoreUnavailable,
    PublicationUnknown,
}

/// Opaque base evidence; paths and provider capabilities are not accepted.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AndroidBaseRef(String);

impl AndroidBaseRef {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, AndroidPrivateStoreError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if valid {
            Ok(Self(value))
        } else {
            Err(AndroidPrivateStoreError::PublicationUnknown)
        }
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AndroidBaseRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AndroidBaseRef([REDACTED])")
    }
}

/// Per-vault private staging and immutable base publication boundary.
///
/// Stage creation is create-no-replace and operation-scoped. Implementations
/// must retain partial bodies: a restart must observe and reconcile them rather
/// than silently replacing evidence from an earlier attempt.
pub(crate) trait AndroidPrivateTransferStore {
    fn load_stage(
        &mut self,
        operation_id: Uuid,
        expected_sha256: &str,
        expected_byte_len: u64,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, AndroidPrivateStoreError>;

    fn create_stage_no_replace(
        &mut self,
        operation_id: Uuid,
        body: Vec<u8>,
        expected_sha256: &str,
        expected_byte_len: u64,
        max_bytes: usize,
    ) -> Result<(), AndroidPrivateStoreError>;

    fn publish_base(
        &mut self,
        operation_id: Uuid,
        sha256_hex: &str,
        body: &[u8],
        max_bytes: usize,
    ) -> Result<AndroidBaseRef, AndroidPrivateStoreError>;
}

/// Android executor with no dependency on current plugin API shapes.
pub(crate) struct AndroidTransferExecutor<V, S> {
    vault: V,
    private_store: S,
    drive: TransferDrive,
}

impl<V, S> AndroidTransferExecutor<V, S>
where
    V: AndroidVaultIo,
    S: AndroidPrivateTransferStore,
{
    pub(crate) const fn new(vault: V, private_store: S, drive: TransferDrive) -> Self {
        Self {
            vault,
            private_store,
            drive,
        }
    }

    fn execute_upload(
        &mut self,
        intent: &TransferIntent,
    ) -> Result<VerifiedTransfer, ExecutionFailure> {
        validate_android_intent(intent)?;
        validate_stage_reference(intent)?;
        let expected_local_revision = intent.expected_local_revision().ok_or_else(|| {
            failure(
                ExecutionFailureKind::NeedsReconcile,
                "local_revision_missing",
                None,
            )
        })?;
        let expected = ExpectedSafEvidence::new(
            intent.sha256_hex(),
            expected_local_revision,
            intent.byte_len(),
        )
        .map_err(policy_failure)?;

        let stage = match self
            .private_store
            .load_stage(
                intent.operation_id(),
                intent.sha256_hex(),
                intent.byte_len(),
                ANDROID_MAX_TRANSFER_BYTES,
            )
            .map_err(private_failure)?
        {
            Some(body) => body,
            None => {
                let body = self
                    .vault
                    .read_exact(intent.path(), ANDROID_MAX_TRANSFER_BYTES)
                    .map_err(vault_failure)?;
                validate_body(
                    AndroidTransferDirection::Upload,
                    intent.path(),
                    &body,
                    &expected,
                )?;
                self.store_new_stage(intent, body)?
            }
        };
        let stage_revision = validate_body(
            AndroidTransferDirection::Upload,
            intent.path(),
            &stage,
            &expected,
        )?;

        // Re-read the exact SAF object after staging. This prevents a remote
        // create from publishing stale bytes after the user changed the file.
        let current = self
            .vault
            .read_exact(intent.path(), ANDROID_MAX_TRANSFER_BYTES)
            .map_err(vault_failure)?;
        validate_body(
            AndroidTransferDirection::Upload,
            intent.path(),
            &current,
            &expected,
        )?;
        if current != stage {
            return Err(reconcile("local_source_changed_after_stage"));
        }

        let display_name = intent
            .path()
            .rsplit('/')
            .next()
            .ok_or_else(|| reconcile("portable_path_invalid"))?;
        if let Some(file_id) = intent.remote_file_id() {
            let revision = intent
                .expected_remote_revision()
                .ok_or_else(|| reconcile("observed_remote_revision_missing"))?;
            self.drive
                .inspect_download_candidate(file_id, intent.parent_id(), display_name, revision)
                .map_err(|error| drive_failure(error, false))?;
        }
        let mime_type = match intent.content_kind() {
            ContentKind::Markdown => "text/markdown",
            ContentKind::Blob => "application/octet-stream",
        };
        let create = CreateIntent::new(
            intent.parent_id(),
            display_name,
            mime_type,
            intent.operation_marker(),
            intent.sha256_hex(),
            intent.byte_len(),
        )
        .map_err(|error| drive_failure(error, false))?;

        let remote = match self
            .drive
            .reconcile_create(create.clone())
            .map_err(|error| drive_failure(error, false))?
        {
            CreateReconciliation::VerifiedExisting(remote) => remote,
            CreateReconciliation::NeedsReconcile(_) => {
                return Err(reconcile("remote_create_conflict"));
            }
            CreateReconciliation::Absent(permit) => {
                let mut session = self
                    .drive
                    .initiate_resumable_create(permit)
                    .map_err(|error| drive_failure(error, true))?;
                let created = loop {
                    let plan =
                        plan_resumable_upload_chunk(session.total_size(), session.next_offset())
                            .map_err(|_| reconcile("transfer_size_invalid"))?;
                    let start = usize::try_from(plan.offset())
                        .map_err(|_| reconcile("transfer_size_invalid"))?;
                    let end = start
                        .checked_add(plan.byte_len())
                        .filter(|end| *end <= stage.len())
                        .ok_or_else(|| reconcile("stage_chunk_length_mismatch"))?;
                    match self.drive.upload_chunk(&mut session, &stage[start..end]) {
                        Ok(UploadProgress::Complete(remote)) => break remote,
                        Ok(UploadProgress::InProgress { .. }) => continue,
                        Err(error) if is_unknown_transport(error.code()) => {
                            match self.drive.query_upload_status(&mut session) {
                                Ok(UploadProgress::Complete(remote)) => break remote,
                                Ok(UploadProgress::InProgress { .. }) => continue,
                                Err(query_error) => {
                                    return Err(drive_failure(query_error, true));
                                }
                            }
                        }
                        Err(error) => return Err(drive_failure(error, true)),
                    }
                };
                self.drive
                    .verify_created_upload(&create, &created)
                    .map_err(|error| drive_failure(error, true))?
            }
        };

        if remote.sha256() != intent.sha256_hex() || remote.size() != intent.byte_len() {
            return Err(reconcile("remote_evidence_mismatch"));
        }
        let base = self
            .private_store
            .publish_base(
                intent.operation_id(),
                intent.sha256_hex(),
                &stage,
                ANDROID_MAX_TRANSFER_BYTES,
            )
            .map_err(private_failure)?;
        verified(
            intent,
            &remote,
            Some(stage_revision.hex),
            base,
            "upload_verified",
        )
    }

    fn execute_download(
        &mut self,
        intent: &TransferIntent,
        before_local_publish: &mut dyn FnMut() -> myvault_transfer::Result<()>,
    ) -> Result<VerifiedTransfer, ExecutionFailure> {
        validate_android_intent(intent)?;
        validate_stage_reference(intent)?;
        let remote_file_id = intent
            .remote_file_id()
            .ok_or_else(|| reconcile("remote_file_missing"))?;
        let remote_revision = intent
            .expected_remote_revision()
            .ok_or_else(|| reconcile("remote_revision_missing"))?;
        let download = DownloadIntent::from_sync_revision(
            remote_file_id,
            intent.parent_id(),
            remote_revision,
            intent.sha256_hex(),
            intent.byte_len(),
        )
        .map_err(|error| drive_failure(error, false))?;

        let stage = match self
            .private_store
            .load_stage(
                intent.operation_id(),
                intent.sha256_hex(),
                intent.byte_len(),
                ANDROID_MAX_TRANSFER_BYTES,
            )
            .map_err(private_failure)?
        {
            Some(body) => {
                validate_download_body(intent, &body)?;
                self.drive
                    .verify_download(&download)
                    .map_err(|error| drive_failure(error, true))?;
                body
            }
            None => self.download_into_new_stage(intent, &download)?,
        };
        let local_revision = validate_download_body(intent, &stage)?;

        before_local_publish().map_err(|_| reconcile("transfer_store_unavailable"))?;
        // The immutable base is durable before create-no-replace. Completion
        // can therefore never point at a local file whose base was not saved.
        let base = self
            .private_store
            .publish_base(
                intent.operation_id(),
                intent.sha256_hex(),
                &stage,
                ANDROID_MAX_TRANSFER_BYTES,
            )
            .map_err(private_failure)?;

        let outcome =
            match self
                .vault
                .create_no_replace(intent.path(), stage, ANDROID_MAX_TRANSFER_BYTES)
            {
                Ok(()) => "download_created_verified",
                Err(AndroidVaultIoError::AlreadyExists) => {
                    let existing = self
                        .vault
                        .read_exact(intent.path(), ANDROID_MAX_TRANSFER_BYTES)
                        .map_err(vault_failure)?;
                    validate_download_body(intent, &existing)
                        .map_err(|_| reconcile("local_create_conflict"))?;
                    "download_existing_verified"
                }
                Err(error) => return Err(vault_failure(error)),
            };

        VerifiedTransfer::new(
            intent.operation_id(),
            remote_file_id,
            remote_revision,
            Some(local_revision.hex),
            intent.sha256_hex(),
            intent.byte_len(),
            base.as_str(),
            outcome,
        )
        .map_err(|_| reconcile("verified_evidence_invalid"))
    }

    fn download_into_new_stage(
        &mut self,
        intent: &TransferIntent,
        download: &DownloadIntent,
    ) -> Result<Vec<u8>, ExecutionFailure> {
        let capacity =
            usize::try_from(intent.byte_len()).map_err(|_| reconcile("transfer_size_invalid"))?;
        let mut body = Vec::with_capacity(capacity);
        if let Err(error) = self.drive.download_blob_to(download, &mut body) {
            let disposition =
                classify_download_failure(error.code(), body.len(), intent.byte_len());
            if disposition.preserve_partial {
                self.private_store
                    .create_stage_no_replace(
                        intent.operation_id(),
                        body,
                        intent.sha256_hex(),
                        intent.byte_len(),
                        ANDROID_MAX_TRANSFER_BYTES,
                    )
                    .or_else(|store_error| match store_error {
                        AndroidPrivateStoreError::AlreadyExists => Ok(()),
                        error => Err(error),
                    })
                    .map_err(private_failure)?;
            }
            return Err(failure(disposition.kind, error.code().as_str(), None));
        }
        validate_download_body(intent, &body)?;
        self.store_new_stage(intent, body)
    }

    fn store_new_stage(
        &mut self,
        intent: &TransferIntent,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, ExecutionFailure> {
        match self.private_store.create_stage_no_replace(
            intent.operation_id(),
            body,
            intent.sha256_hex(),
            intent.byte_len(),
            ANDROID_MAX_TRANSFER_BYTES,
        ) {
            Ok(()) | Err(AndroidPrivateStoreError::AlreadyExists) => self
                .private_store
                .load_stage(
                    intent.operation_id(),
                    intent.sha256_hex(),
                    intent.byte_len(),
                    ANDROID_MAX_TRANSFER_BYTES,
                )
                .map_err(private_failure)?
                .ok_or_else(|| reconcile("local_stage_unavailable")),
            Err(error) => Err(private_failure(error)),
        }
    }
}

impl<V, S> TransferExecutor for AndroidTransferExecutor<V, S>
where
    V: AndroidVaultIo,
    S: AndroidPrivateTransferStore,
{
    fn execute(
        &mut self,
        intent: &TransferIntent,
        before_local_publish: &mut dyn FnMut() -> myvault_transfer::Result<()>,
    ) -> Result<VerifiedTransfer, ExecutionFailure> {
        match intent.direction() {
            TransferDirection::Upload => self.execute_upload(intent),
            TransferDirection::Download => self.execute_download(intent, before_local_publish),
        }
    }
}

fn validate_android_intent(intent: &TransferIntent) -> Result<(), ExecutionFailure> {
    if intent.byte_len() > ANDROID_MAX_TRANSFER_BYTES as u64 {
        Err(reconcile("android_transfer_too_large"))
    } else {
        Ok(())
    }
}

fn validate_stage_reference(intent: &TransferIntent) -> Result<(), ExecutionFailure> {
    let expected = format!("stage-{}", intent.operation_id());
    if intent.stage_ref() == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(reconcile("stage_reference_mismatch"))
    }
}

fn validate_download_body(
    intent: &TransferIntent,
    body: &[u8],
) -> Result<FileRevision, ExecutionFailure> {
    if body.len() > ANDROID_MAX_TRANSFER_BYTES {
        return Err(reconcile("android_transfer_too_large"));
    }
    let revision = FileRevision::from_bytes(body);
    let expected =
        ExpectedSafEvidence::new(intent.sha256_hex(), revision.hex.clone(), intent.byte_len())
            .map_err(policy_failure)?;
    validate_body(
        AndroidTransferDirection::Download,
        intent.path(),
        body,
        &expected,
    )
}

fn validate_body(
    direction: AndroidTransferDirection,
    path: &str,
    body: &[u8],
    expected: &ExpectedSafEvidence,
) -> Result<FileRevision, ExecutionFailure> {
    prepare_saf_transfer(direction, path, body, expected)
        .map(|evidence| evidence.revision().clone())
        .map_err(policy_failure)
}

fn verified(
    intent: &TransferIntent,
    remote: &RemoteObject,
    local_revision: Option<String>,
    base: AndroidBaseRef,
    outcome: &'static str,
) -> Result<VerifiedTransfer, ExecutionFailure> {
    VerifiedTransfer::new(
        intent.operation_id(),
        remote.file_id(),
        remote.sync_revision(),
        local_revision,
        intent.sha256_hex(),
        intent.byte_len(),
        base.as_str(),
        outcome,
    )
    .map_err(|_| reconcile("verified_evidence_invalid"))
}

fn policy_failure(_error: AndroidTransferPolicyError) -> ExecutionFailure {
    reconcile("android_policy_rejected")
}

fn vault_failure(error: AndroidVaultIoError) -> ExecutionFailure {
    let code = match error {
        AndroidVaultIoError::AlreadyExists => "android_local_already_exists",
        AndroidVaultIoError::NotFound => "android_local_not_found",
        AndroidVaultIoError::InvalidRequest => "android_local_invalid_request",
        AndroidVaultIoError::ProtectedPath => "android_local_protected_path",
        AndroidVaultIoError::ResourceLimit => "android_local_resource_limit",
        AndroidVaultIoError::VaultUnavailable => "android_vault_unavailable",
        AndroidVaultIoError::PublicationUnknown => "android_local_publication_unknown",
    };
    reconcile(code)
}

fn private_failure(error: AndroidPrivateStoreError) -> ExecutionFailure {
    let code = match error {
        AndroidPrivateStoreError::AlreadyExists => "android_private_stage_exists",
        AndroidPrivateStoreError::StageUnavailable => "android_private_stage_unavailable",
        AndroidPrivateStoreError::ResourceLimit => "android_private_resource_limit",
        AndroidPrivateStoreError::StoreUnavailable => "android_private_store_unavailable",
        AndroidPrivateStoreError::PublicationUnknown => "android_private_publication_unknown",
    };
    reconcile(code)
}

fn is_unknown_transport(code: DriveErrorCode) -> bool {
    matches!(
        code,
        DriveErrorCode::Transport | DriveErrorCode::Timeout | DriveErrorCode::TransientProvider
    )
}

fn drive_failure(error: DriveError, side_effect_possible: bool) -> ExecutionFailure {
    let kind = classify_drive_failure(error.code(), side_effect_possible);
    let retry_after = error
        .retry_after_seconds()
        .map(Duration::from_secs)
        .filter(|_| kind == ExecutionFailureKind::RateLimited);
    failure(kind, error.code().as_str(), retry_after)
}

fn classify_drive_failure(
    code: DriveErrorCode,
    side_effect_possible: bool,
) -> ExecutionFailureKind {
    match code {
        DriveErrorCode::Unauthorized if side_effect_possible => {
            ExecutionFailureKind::NeedsReconcile
        }
        DriveErrorCode::Unauthorized => ExecutionFailureKind::AuthRequired,
        DriveErrorCode::RateLimited => ExecutionFailureKind::RateLimited,
        DriveErrorCode::Transport if !side_effect_possible => ExecutionFailureKind::Offline,
        code if is_unknown_transport(code) && side_effect_possible => {
            ExecutionFailureKind::TransientUnknown
        }
        code if is_unknown_transport(code) => ExecutionFailureKind::TransientSafe,
        _ => ExecutionFailureKind::NeedsReconcile,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DownloadFailureDisposition {
    kind: ExecutionFailureKind,
    preserve_partial: bool,
}

fn classify_download_failure(
    code: DriveErrorCode,
    bytes_written: usize,
    expected_bytes: u64,
) -> DownloadFailureDisposition {
    if bytes_written == 0 && expected_bytes != 0 {
        let kind = match code {
            DriveErrorCode::Unauthorized => Some(ExecutionFailureKind::AuthRequired),
            DriveErrorCode::Transport => Some(ExecutionFailureKind::Offline),
            DriveErrorCode::Timeout | DriveErrorCode::TransientProvider => {
                Some(ExecutionFailureKind::TransientSafe)
            }
            _ => None,
        };
        if let Some(kind) = kind {
            return DownloadFailureDisposition {
                kind,
                preserve_partial: false,
            };
        }
    }
    DownloadFailureDisposition {
        kind: ExecutionFailureKind::NeedsReconcile,
        preserve_partial: bytes_written != 0,
    }
}

fn reconcile(code: &'static str) -> ExecutionFailure {
    failure(ExecutionFailureKind::NeedsReconcile, code, None)
}

fn failure(
    kind: ExecutionFailureKind,
    code: &'static str,
    retry_after: Option<Duration>,
) -> ExecutionFailure {
    ExecutionFailure::new(kind, code, retry_after)
        .expect("Android transfer classifications are compile-time bounded")
}

#[cfg(target_os = "android")]
mod native_adapters {
    use super::*;
    use std::io::Write;
    use tauri_plugin_private_root::{
        AndroidTransferStore, TransferStoreError, MAX_ANDROID_TRANSFER_BYTES,
    };
    use tauri_plugin_vault_saf::{SafTransferError, SafVaultCapability, VaultSafExt};

    pub(crate) struct AndroidSafVaultIo {
        app: tauri::AppHandle,
        vault: SafVaultCapability,
    }

    impl AndroidSafVaultIo {
        pub(crate) const fn new(app: tauri::AppHandle, vault: SafVaultCapability) -> Self {
            Self { app, vault }
        }
    }

    impl AndroidVaultIo for AndroidSafVaultIo {
        fn read_exact(
            &mut self,
            portable_path: &str,
            max_bytes: usize,
        ) -> Result<Vec<u8>, AndroidVaultIoError> {
            let binary = self
                .app
                .vault_saf()
                .read_binary(&self.vault, portable_path, max_bytes)
                .map_err(map_saf_error)?;
            let digest = myvault_core::Sha256Digest::from_bytes(&binary.bytes);
            if binary.byte_len != binary.bytes.len() as u64
                || binary.revision_hex != digest.as_str()
            {
                return Err(AndroidVaultIoError::PublicationUnknown);
            }
            Ok(binary.bytes)
        }

        fn create_no_replace(
            &mut self,
            portable_path: &str,
            body: Vec<u8>,
            max_bytes: usize,
        ) -> Result<(), AndroidVaultIoError> {
            if body.len() > max_bytes {
                return Err(AndroidVaultIoError::ResourceLimit);
            }
            let digest = myvault_core::Sha256Digest::from_bytes(&body);
            let saved = self
                .app
                .vault_saf()
                .create_binary(&self.vault, portable_path, &body, digest.as_str())
                .map_err(map_saf_error)?;
            if saved.byte_len != body.len() as u64 || saved.revision_hex != digest.as_str() {
                return Err(AndroidVaultIoError::PublicationUnknown);
            }
            Ok(())
        }
    }

    pub(crate) struct AndroidPrivateStoreAdapter {
        store: AndroidTransferStore,
    }

    impl AndroidPrivateStoreAdapter {
        pub(crate) const fn new(store: AndroidTransferStore) -> Self {
            Self { store }
        }

        fn verified_stage(
            &self,
            operation_id: Uuid,
            expected_sha256: &str,
            expected_byte_len: u64,
        ) -> Result<Option<tauri_plugin_private_root::VerifiedAndroidStage>, AndroidPrivateStoreError>
        {
            match self
                .store
                .load_verified_stage(operation_id, expected_sha256, expected_byte_len)
            {
                Ok(stage) => Ok(Some(stage)),
                Err(TransferStoreError::StageUnavailable) => Ok(None),
                Err(TransferStoreError::DigestMismatch) => {
                    match self.store.discard_incomplete_stage(
                        operation_id,
                        expected_sha256,
                        expected_byte_len,
                    ) {
                        Ok(()) => Ok(None),
                        Err(error) => Err(map_store_error(error)),
                    }
                }
                Err(error) => Err(map_store_error(error)),
            }
        }
    }

    impl AndroidPrivateTransferStore for AndroidPrivateStoreAdapter {
        fn load_stage(
            &mut self,
            operation_id: Uuid,
            expected_sha256: &str,
            expected_byte_len: u64,
            max_bytes: usize,
        ) -> Result<Option<Vec<u8>>, AndroidPrivateStoreError> {
            if expected_byte_len > max_bytes as u64
                || expected_byte_len > MAX_ANDROID_TRANSFER_BYTES
            {
                return Err(AndroidPrivateStoreError::ResourceLimit);
            }
            self.verified_stage(operation_id, expected_sha256, expected_byte_len)?
                .map(|stage| {
                    self.store
                        .read_verified_stage(&stage)
                        .map_err(map_store_error)
                })
                .transpose()
        }

        fn create_stage_no_replace(
            &mut self,
            operation_id: Uuid,
            body: Vec<u8>,
            expected_sha256: &str,
            expected_byte_len: u64,
            max_bytes: usize,
        ) -> Result<(), AndroidPrivateStoreError> {
            if body.len() > max_bytes
                || body.len() as u64 > expected_byte_len
                || expected_byte_len > MAX_ANDROID_TRANSFER_BYTES
            {
                return Err(AndroidPrivateStoreError::ResourceLimit);
            }
            let mut writer = self
                .store
                .begin_stage(operation_id)
                .map_err(map_store_error)?;
            writer
                .write_all(&body)
                .map_err(|_| AndroidPrivateStoreError::PublicationUnknown)?;
            if body.len() as u64 == expected_byte_len {
                self.store
                    .finish_stage(writer, expected_sha256, expected_byte_len)
                    .map_err(map_store_error)?;
            }
            Ok(())
        }

        fn publish_base(
            &mut self,
            operation_id: Uuid,
            sha256_hex: &str,
            body: &[u8],
            max_bytes: usize,
        ) -> Result<AndroidBaseRef, AndroidPrivateStoreError> {
            if body.len() > max_bytes {
                return Err(AndroidPrivateStoreError::ResourceLimit);
            }
            let stage = self
                .verified_stage(operation_id, sha256_hex, body.len() as u64)?
                .ok_or(AndroidPrivateStoreError::StageUnavailable)?;
            let staged = self
                .store
                .read_verified_stage(&stage)
                .map_err(map_store_error)?;
            if staged != body {
                return Err(AndroidPrivateStoreError::PublicationUnknown);
            }
            let base = self.store.publish_base(&stage).map_err(map_store_error)?;
            if base.byte_len() != body.len() as u64 {
                return Err(AndroidPrivateStoreError::PublicationUnknown);
            }
            AndroidBaseRef::new(base.opaque_ref())
        }
    }

    fn map_saf_error(error: SafTransferError) -> AndroidVaultIoError {
        match error {
            SafTransferError::AlreadyExists => AndroidVaultIoError::AlreadyExists,
            SafTransferError::NotFound => AndroidVaultIoError::NotFound,
            SafTransferError::InvalidPath | SafTransferError::InvalidRequest => {
                AndroidVaultIoError::InvalidRequest
            }
            SafTransferError::ResourceLimit => AndroidVaultIoError::ResourceLimit,
            SafTransferError::VaultUnavailable => AndroidVaultIoError::VaultUnavailable,
            SafTransferError::StaleRevision
            | SafTransferError::DigestMismatch
            | SafTransferError::UnsupportedReplace
            | SafTransferError::WriteOutcomeUnknown
            | SafTransferError::NativeBridge => AndroidVaultIoError::PublicationUnknown,
        }
    }

    fn map_store_error(error: TransferStoreError) -> AndroidPrivateStoreError {
        match error {
            TransferStoreError::StageCollision => AndroidPrivateStoreError::AlreadyExists,
            TransferStoreError::StageUnavailable => AndroidPrivateStoreError::StageUnavailable,
            TransferStoreError::ResourceLimit => AndroidPrivateStoreError::ResourceLimit,
            TransferStoreError::InvalidVaultId
            | TransferStoreError::InvalidOperationId
            | TransferStoreError::InvalidDigest => AndroidPrivateStoreError::StoreUnavailable,
            TransferStoreError::DigestMismatch
            | TransferStoreError::EvidencePreserved
            | TransferStoreError::EvidenceAmbiguous
            | TransferStoreError::Io(_)
            | TransferStoreError::PrivateStorage(_) => AndroidPrivateStoreError::PublicationUnknown,
        }
    }
}

#[cfg(target_os = "android")]
pub(crate) use native_adapters::{AndroidPrivateStoreAdapter, AndroidSafVaultIo};

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use myvault_core::Sha256Digest;
    use std::{cell::RefCell, collections::BTreeMap, io, net::TcpListener, rc::Rc};

    const ROOT_JSON: &str = r#"{"id":"root_1","name":"Root","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#;

    #[derive(Default)]
    struct FakeVault {
        files: BTreeMap<String, Vec<u8>>,
        events: Rc<RefCell<Vec<&'static str>>>,
        read_count: usize,
        create_count: usize,
    }

    impl AndroidVaultIo for FakeVault {
        fn read_exact(
            &mut self,
            portable_path: &str,
            max_bytes: usize,
        ) -> Result<Vec<u8>, AndroidVaultIoError> {
            self.read_count += 1;
            let body = self
                .files
                .get(portable_path)
                .cloned()
                .ok_or(AndroidVaultIoError::NotFound)?;
            if body.len() > max_bytes {
                return Err(AndroidVaultIoError::ResourceLimit);
            }
            Ok(body)
        }

        fn create_no_replace(
            &mut self,
            portable_path: &str,
            body: Vec<u8>,
            max_bytes: usize,
        ) -> Result<(), AndroidVaultIoError> {
            self.create_count += 1;
            self.events.borrow_mut().push("local_create");
            if body.len() > max_bytes {
                return Err(AndroidVaultIoError::ResourceLimit);
            }
            if self.files.contains_key(portable_path) {
                return Err(AndroidVaultIoError::AlreadyExists);
            }
            self.files.insert(portable_path.to_owned(), body);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakePrivateStore {
        stages: BTreeMap<Uuid, Vec<u8>>,
        bases: BTreeMap<Uuid, Vec<u8>>,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl AndroidPrivateTransferStore for FakePrivateStore {
        fn load_stage(
            &mut self,
            operation_id: Uuid,
            expected_sha256: &str,
            expected_byte_len: u64,
            max_bytes: usize,
        ) -> Result<Option<Vec<u8>>, AndroidPrivateStoreError> {
            let value = self.stages.get(&operation_id).cloned();
            if value.as_ref().is_some_and(|body| body.len() > max_bytes) {
                return Err(AndroidPrivateStoreError::ResourceLimit);
            }
            if value
                .as_ref()
                .is_some_and(|body| (body.len() as u64) < expected_byte_len)
            {
                self.stages.remove(&operation_id);
                return Ok(None);
            }
            if value.as_ref().is_some_and(|body| {
                body.len() as u64 != expected_byte_len
                    || Sha256Digest::from_bytes(body).as_str() != expected_sha256
            }) {
                return Err(AndroidPrivateStoreError::PublicationUnknown);
            }
            Ok(value)
        }

        fn create_stage_no_replace(
            &mut self,
            operation_id: Uuid,
            body: Vec<u8>,
            expected_sha256: &str,
            expected_byte_len: u64,
            max_bytes: usize,
        ) -> Result<(), AndroidPrivateStoreError> {
            if body.len() > max_bytes {
                return Err(AndroidPrivateStoreError::ResourceLimit);
            }
            if self.stages.contains_key(&operation_id) {
                return Err(AndroidPrivateStoreError::AlreadyExists);
            }
            if body.len() as u64 == expected_byte_len
                && Sha256Digest::from_bytes(&body).as_str() != expected_sha256
            {
                return Err(AndroidPrivateStoreError::PublicationUnknown);
            }
            self.stages.insert(operation_id, body);
            Ok(())
        }

        fn publish_base(
            &mut self,
            operation_id: Uuid,
            sha256_hex: &str,
            body: &[u8],
            max_bytes: usize,
        ) -> Result<AndroidBaseRef, AndroidPrivateStoreError> {
            if body.len() > max_bytes || Sha256Digest::from_bytes(body).as_str() != sha256_hex {
                return Err(AndroidPrivateStoreError::PublicationUnknown);
            }
            self.events.borrow_mut().push("base");
            self.bases.insert(operation_id, body.to_vec());
            AndroidBaseRef::new(format!("base-{}", operation_id.simple()))
        }
    }

    fn evidence(bytes: &[u8]) -> (String, String) {
        (
            Sha256Digest::from_bytes(bytes).as_str().to_owned(),
            FileRevision::from_bytes(bytes).hex,
        )
    }

    fn remote_revision() -> String {
        format!("{:064x}", 2)
    }

    fn download_intent(operation_id: Uuid, bytes: &[u8]) -> TransferIntent {
        let (sha256, _) = evidence(bytes);
        TransferIntent::new(
            operation_id,
            TransferDirection::Download,
            "note.md",
            "root_1",
            Some("file_1".to_owned()),
            None,
            Some(remote_revision()),
            sha256,
            bytes.len() as u64,
            ContentKind::Markdown,
            format!("r2-{}", operation_id.simple()),
            Some(format!("stage-{operation_id}")),
            None,
            0,
        )
        .expect("download intent")
    }

    fn upload_intent(operation_id: Uuid, bytes: &[u8]) -> TransferIntent {
        let (sha256, revision) = evidence(bytes);
        TransferIntent::new(
            operation_id,
            TransferDirection::Upload,
            "note.md",
            "root_1",
            None,
            Some(revision),
            None,
            sha256,
            bytes.len() as u64,
            ContentKind::Markdown,
            format!("r2-{}", operation_id.simple()),
            Some(format!("stage-{operation_id}")),
            None,
            0,
        )
        .expect("upload intent")
    }

    fn drive(server: &Server) -> TransferDrive {
        let origin = server.url();
        TransferDrive::for_test_origins(
            &format!("{origin}/drive/v3/"),
            &format!("{origin}/upload/drive/v3/"),
            "account_1",
            "root_1",
            ANDROID_MAX_TRANSFER_BYTES as u64,
        )
        .expect("test Drive")
    }

    fn disconnected_drive() -> TransferDrive {
        let listener = TcpListener::bind("127.0.0.1:0").expect("unused local port");
        let address = listener.local_addr().expect("local address");
        drop(listener);
        let origin = format!("http://{address}");
        TransferDrive::for_test_origins(
            &format!("{origin}/drive/v3/"),
            &format!("{origin}/upload/drive/v3/"),
            "account_1",
            "root_1",
            ANDROID_MAX_TRANSFER_BYTES as u64,
        )
        .expect("disconnected test Drive")
    }

    fn mock_binding(server: &mut Server, count: usize) {
        server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
            .expect(count)
            .create();
        server
            .mock("GET", "/drive/v3/files/root_1")
            .match_query(Matcher::Any)
            .with_body(ROOT_JSON)
            .expect(count)
            .create();
    }

    fn remote_json(bytes: &[u8], marker: Option<&str>) -> String {
        let sha256 = Sha256Digest::from_bytes(bytes);
        let properties = marker.map_or_else(String::new, |marker| {
            format!(
                r#", "appProperties":{{"myvaultOperation":"{marker}","myvaultSha256":"{}","myvaultSize":"{}"}}"#,
                sha256.as_str(),
                bytes.len()
            )
        });
        format!(
            r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"{}","sha256Checksum":"{}"{properties}}}"#,
            bytes.len(),
            sha256.as_str()
        )
    }

    fn mock_successful_download(server: &mut Server, bytes: &[u8]) {
        mock_binding(server, 2);
        let remote = remote_json(bytes, None);
        server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(remote)
            .expect(2)
            .create();
        server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
            .with_body(bytes)
            .expect(1)
            .create();
    }

    #[test]
    fn upload_uses_vec_stage_and_publishes_verified_base() {
        let bytes = b"abc";
        let operation_id = Uuid::new_v4();
        let intent = upload_intent(operation_id, bytes);
        let mut server = Server::new();
        mock_binding(&mut server, 8);
        let initial_marker = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "100".into()),
                Matcher::Regex("myvaultOperation".into()),
            ]))
            .with_body(r#"{"files":[]}"#)
            .create();
        let initial_name = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "100".into()),
                Matcher::Regex("name".into()),
            ]))
            .with_body(r#"{"files":[]}"#)
            .create();
        let marker = intent.operation_marker().to_owned();
        let remote = remote_json(bytes, Some(&marker));
        let origin = server.url();
        let initiate = server
            .mock("POST", "/upload/drive/v3/files")
            .match_query(Matcher::UrlEncoded("uploadType".into(), "resumable".into()))
            .with_status(200)
            .with_header(
                "location",
                &format!("{origin}/upload/drive/v3/files?upload_id=session"),
            )
            .create();
        let upload = server
            .mock("PUT", "/upload/drive/v3/files")
            .match_query(Matcher::UrlEncoded("upload_id".into(), "session".into()))
            .match_header("content-range", "bytes 0-2/3")
            .match_body("abc")
            .with_status(200)
            .with_body(remote.clone())
            .create();
        let exact_metadata = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(remote.clone())
            .expect(2)
            .create();
        let final_marker = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "2".into()),
                Matcher::Regex("myvaultOperation".into()),
            ]))
            .with_body(format!(r#"{{"files":[{remote}]}}"#))
            .create();
        let final_name = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "2".into()),
                Matcher::Regex("name".into()),
            ]))
            .with_body(format!(r#"{{"files":[{remote}]}}"#))
            .create();

        let mut vault = FakeVault::default();
        vault.files.insert("note.md".into(), bytes.to_vec());
        let store = FakePrivateStore::default();
        let mut executor = AndroidTransferExecutor::new(vault, store, drive(&server));
        let verified = executor
            .execute(&intent, &mut || Ok(()))
            .expect("verified upload");

        assert_eq!(verified.outcome_code(), "upload_verified");
        assert_eq!(executor.private_store.stages[&operation_id], bytes);
        assert_eq!(executor.private_store.bases[&operation_id], bytes);
        assert_eq!(executor.vault.read_count, 2);
        initial_marker.assert();
        initial_name.assert();
        initiate.assert();
        upload.assert();
        exact_metadata.assert();
        final_marker.assert();
        final_name.assert();
    }

    #[test]
    fn download_creates_without_replace_after_base_publication() {
        let bytes = b"abc";
        let operation_id = Uuid::new_v4();
        let intent = download_intent(operation_id, bytes);
        let mut server = Server::new();
        mock_successful_download(&mut server, bytes);
        let events = Rc::new(RefCell::new(Vec::new()));
        let vault = FakeVault {
            events: Rc::clone(&events),
            ..FakeVault::default()
        };
        let store = FakePrivateStore {
            events: Rc::clone(&events),
            ..FakePrivateStore::default()
        };
        let mut executor = AndroidTransferExecutor::new(vault, store, drive(&server));
        let callback_events = Rc::clone(&events);
        let verified = executor
            .execute(&intent, &mut || {
                callback_events.borrow_mut().push("before_publish");
                Ok(())
            })
            .expect("verified download");

        assert_eq!(verified.outcome_code(), "download_created_verified");
        assert_eq!(executor.vault.files["note.md"], bytes);
        assert_eq!(executor.private_store.bases[&operation_id], bytes);
        assert_eq!(
            events.borrow().as_slice(),
            ["before_publish", "base", "local_create"]
        );
    }

    #[test]
    fn existing_same_bytes_complete_idempotently_but_different_bytes_reconcile() {
        for (local, expected_outcome) in [
            (b"abc".as_slice(), Some("download_existing_verified")),
            (b"xyz".as_slice(), None),
        ] {
            let bytes = b"abc";
            let operation_id = Uuid::new_v4();
            let intent = download_intent(operation_id, bytes);
            let mut server = Server::new();
            mock_successful_download(&mut server, bytes);
            let mut vault = FakeVault::default();
            vault.files.insert("note.md".into(), local.to_vec());
            let mut executor =
                AndroidTransferExecutor::new(vault, FakePrivateStore::default(), drive(&server));
            let result = executor.execute(&intent, &mut || Ok(()));
            if let Some(outcome) = expected_outcome {
                assert_eq!(result.unwrap().outcome_code(), outcome);
            } else {
                let failure = result.unwrap_err();
                assert_eq!(failure.kind(), ExecutionFailureKind::NeedsReconcile);
                assert_eq!(failure.code(), "local_create_conflict");
            }
            assert_eq!(executor.vault.files["note.md"], local);
            assert_eq!(executor.vault.create_count, 1);
        }
    }

    #[test]
    fn interrupted_download_preserves_then_safely_discards_short_stage_on_restart() {
        let bytes = b"abc";
        let operation_id = Uuid::new_v4();
        let intent = download_intent(operation_id, bytes);
        let mut server = Server::new();
        mock_binding(&mut server, 1);
        server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(remote_json(bytes, None))
            .expect(1)
            .create();
        server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
            .with_chunked_body(|writer| {
                writer.write_all(b"a")?;
                Err(io::Error::new(io::ErrorKind::ConnectionAborted, "cut"))
            })
            .expect(1)
            .create();
        let mut executor = AndroidTransferExecutor::new(
            FakeVault::default(),
            FakePrivateStore::default(),
            drive(&server),
        );

        let first = executor.execute(&intent, &mut || Ok(())).unwrap_err();
        assert!(matches!(
            first.kind(),
            ExecutionFailureKind::Offline | ExecutionFailureKind::NeedsReconcile
        ));
        executor
            .private_store
            .stages
            .entry(operation_id)
            .or_insert_with(|| b"a".to_vec());
        assert_eq!(executor.private_store.stages[&operation_id], b"a");

        let mut restarted_server = Server::new();
        mock_successful_download(&mut restarted_server, bytes);
        let mut executor = AndroidTransferExecutor::new(
            executor.vault,
            executor.private_store,
            drive(&restarted_server),
        );
        let second = executor.execute(&intent, &mut || Ok(())).unwrap();
        assert_eq!(second.outcome_code(), "download_created_verified");
        assert_eq!(executor.private_store.stages[&operation_id], bytes);
        assert_eq!(executor.vault.files["note.md"], bytes);
    }

    #[test]
    fn auth_and_offline_before_remote_mutation_publish_no_base() {
        let bytes = b"abc";
        let operation_id = Uuid::new_v4();
        let intent = upload_intent(operation_id, bytes);

        let mut auth_server = Server::new();
        mock_binding(&mut auth_server, 1);
        auth_server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Regex("myvaultOperation".into()))
            .with_status(401)
            .create();
        let mut auth_vault = FakeVault::default();
        auth_vault.files.insert("note.md".into(), bytes.to_vec());
        let mut auth_executor = AndroidTransferExecutor::new(
            auth_vault,
            FakePrivateStore::default(),
            drive(&auth_server),
        );
        let auth = auth_executor.execute(&intent, &mut || Ok(())).unwrap_err();
        assert_eq!(auth.kind(), ExecutionFailureKind::AuthRequired);
        assert!(auth_executor.private_store.bases.is_empty());
        assert_eq!(auth_executor.vault.create_count, 0);

        let mut offline_vault = FakeVault::default();
        offline_vault.files.insert("note.md".into(), bytes.to_vec());
        let mut offline_executor = AndroidTransferExecutor::new(
            offline_vault,
            FakePrivateStore::default(),
            disconnected_drive(),
        );
        let offline = offline_executor
            .execute(&intent, &mut || Ok(()))
            .unwrap_err();
        assert_eq!(offline.kind(), ExecutionFailureKind::Offline);
        assert!(offline_executor.private_store.bases.is_empty());
        assert_eq!(offline_executor.vault.create_count, 0);
    }

    #[test]
    fn completed_private_stage_is_reverified_remotely_after_restart() {
        let bytes = b"abc";
        let operation_id = Uuid::new_v4();
        let intent = download_intent(operation_id, bytes);
        let mut server = Server::new();
        mock_binding(&mut server, 1);
        server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(remote_json(bytes, None))
            .expect(1)
            .create();
        let mut store = FakePrivateStore::default();
        store.stages.insert(operation_id, bytes.to_vec());
        let mut executor =
            AndroidTransferExecutor::new(FakeVault::default(), store, drive(&server));

        let verified = executor
            .execute(&intent, &mut || Ok(()))
            .expect("restart completion");
        assert_eq!(verified.outcome_code(), "download_created_verified");
        assert_eq!(executor.vault.files["note.md"], bytes);
    }

    #[test]
    fn cap_is_rejected_before_local_or_remote_side_effects() {
        let operation_id = Uuid::new_v4();
        let sha256 = Sha256Digest::from_bytes(b"").as_str().to_owned();
        let intent = TransferIntent::new(
            operation_id,
            TransferDirection::Download,
            "large.bin",
            "root_1",
            Some("file_1".into()),
            None,
            Some(remote_revision()),
            sha256,
            ANDROID_MAX_TRANSFER_BYTES as u64 + 1,
            ContentKind::Blob,
            format!("r2-{}", operation_id.simple()),
            Some(format!("stage-{operation_id}")),
            None,
            0,
        )
        .expect("core-sized intent");
        let mut server = Server::new();
        let mut executor = AndroidTransferExecutor::new(
            FakeVault::default(),
            FakePrivateStore::default(),
            drive(&server),
        );
        let failure = executor.execute(&intent, &mut || Ok(())).unwrap_err();
        assert_eq!(failure.kind(), ExecutionFailureKind::NeedsReconcile);
        assert_eq!(failure.code(), "android_transfer_too_large");
        assert_eq!(executor.vault.read_count, 0);
        assert!(executor.private_store.stages.is_empty());
        assert!(executor.private_store.bases.is_empty());
        // Keep the server alive until after execution; zero mocks prove zero HTTP.
        server.reset();
    }

    #[test]
    fn local_error_codes_and_base_refs_are_redacted() {
        for error in [
            AndroidVaultIoError::ProtectedPath,
            AndroidVaultIoError::PublicationUnknown,
            AndroidVaultIoError::VaultUnavailable,
        ] {
            let mapped = vault_failure(error);
            assert_eq!(mapped.kind(), ExecutionFailureKind::NeedsReconcile);
            assert!(!mapped.code().contains('/'));
            assert!(!mapped.code().to_ascii_lowercase().contains("bearer"));
        }
        let base = AndroidBaseRef::new("base-safe_1.0").unwrap();
        assert_eq!(format!("{base:?}"), "AndroidBaseRef([REDACTED])");
        assert!(AndroidBaseRef::new("/private/path").is_err());
        assert!(AndroidBaseRef::new("Bearer token").is_err());
    }
}
