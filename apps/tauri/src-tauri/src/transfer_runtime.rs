#![cfg(not(target_os = "android"))]

use myvault_app_service::{AppService, NativeTransferError, TransferStageRef, VaultSessionId};
use myvault_drive::{
    CreateIntent, CreateReconciliation, DownloadIntent, Error as DriveError,
    ErrorCode as DriveErrorCode, RemoteObject, TransferDrive, UploadProgress,
};
use myvault_transfer::{
    ContentKind, ExecutionFailure, ExecutionFailureKind, TransferDirection, TransferExecutor,
    TransferIntent, VerifiedTransfer, MAX_TRANSFER_BYTES,
};
use std::time::Duration;

const UPLOAD_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const MAX_TRANSFER_BYTES_USIZE: usize = MAX_TRANSFER_BYTES as usize;

/// Native-only bridge between durable transfer intent and the two narrow
/// capabilities that may touch content. It owns no ambient path, credential,
/// upload-session URI, or frontend-serializable body.
pub(crate) struct NativeTransferExecutor<'a> {
    service: &'a AppService,
    session_id: VaultSessionId,
    drive: TransferDrive,
}

impl<'a> NativeTransferExecutor<'a> {
    pub(crate) const fn new(
        service: &'a AppService,
        session_id: VaultSessionId,
        drive: TransferDrive,
    ) -> Self {
        Self {
            service,
            session_id,
            drive,
        }
    }

    fn execute_upload(
        &mut self,
        intent: &TransferIntent,
    ) -> Result<VerifiedTransfer, ExecutionFailure> {
        let expected_stage = format!("stage-{}", intent.operation_id());
        if intent.stage_ref() != Some(expected_stage.as_str()) {
            return Err(failure(
                ExecutionFailureKind::NeedsReconcile,
                "stage_reference_mismatch",
                None,
            ));
        }
        let expected_local_revision = intent.expected_local_revision().ok_or_else(|| {
            failure(
                ExecutionFailureKind::NeedsReconcile,
                "local_revision_missing",
                None,
            )
        })?;
        let stage = match self.service.load_transfer_stage(
            self.session_id,
            intent.operation_id(),
            intent.sha256_hex(),
            intent.byte_len(),
            MAX_TRANSFER_BYTES_USIZE,
        ) {
            Ok(stage) => stage,
            Err(NativeTransferError::StageUnavailable) => self
                .service
                .stage_transfer_source(
                    self.session_id,
                    intent.operation_id(),
                    intent.path(),
                    MAX_TRANSFER_BYTES_USIZE,
                )
                .map_err(local_failure)?,
            Err(error) => return Err(local_failure(error)),
        };
        validate_stage(intent, &stage)?;
        if stage.snapshot().revision.hex != expected_local_revision {
            return Err(failure(
                ExecutionFailureKind::NeedsReconcile,
                "local_source_changed",
                None,
            ));
        }
        let mut sink = std::io::sink();
        let current_source = self
            .service
            .stream_transfer_source(
                self.session_id,
                intent.path(),
                &mut sink,
                MAX_TRANSFER_BYTES_USIZE,
            )
            .map_err(local_failure)?;
        if current_source != *stage.snapshot() {
            return Err(failure(
                ExecutionFailureKind::NeedsReconcile,
                "local_source_changed_after_stage",
                None,
            ));
        }

        let display_name = intent.path().rsplit('/').next().ok_or_else(|| {
            failure(
                ExecutionFailureKind::NeedsReconcile,
                "portable_path_invalid",
                None,
            )
        })?;
        let mime_type = match intent.content_kind() {
            ContentKind::Markdown => "text/markdown",
            ContentKind::Blob => "application/octet-stream",
        };
        if let Some(observed_file_id) = intent.remote_file_id() {
            let observed_revision = intent.expected_remote_revision().ok_or_else(|| {
                failure(
                    ExecutionFailureKind::NeedsReconcile,
                    "observed_remote_revision_missing",
                    None,
                )
            })?;
            self.drive
                .inspect_download_candidate(
                    observed_file_id,
                    intent.parent_id(),
                    display_name,
                    observed_revision,
                )
                .map_err(|error| drive_failure(error, false))?;
        }
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
                return Err(failure(
                    ExecutionFailureKind::NeedsReconcile,
                    "remote_create_conflict",
                    None,
                ));
            }
            CreateReconciliation::Absent(permit) => {
                let mut session = self
                    .drive
                    .initiate_resumable_create(permit)
                    .map_err(|error| drive_failure(error, true))?;
                let created = loop {
                    let offset = session.next_offset();
                    let remaining = session.total_size().saturating_sub(offset);
                    let requested = usize::try_from(remaining.min(UPLOAD_CHUNK_BYTES as u64))
                        .map_err(|_| {
                            failure(
                                ExecutionFailureKind::NeedsReconcile,
                                "transfer_size_invalid",
                                None,
                            )
                        })?;
                    let bytes = self
                        .service
                        .read_verified_stage_chunk(
                            self.session_id,
                            &stage,
                            offset,
                            if session.total_size() == 0 {
                                1
                            } else {
                                requested
                            },
                            MAX_TRANSFER_BYTES_USIZE,
                        )
                        .map_err(local_failure)?;
                    if bytes.len() != requested {
                        return Err(failure(
                            ExecutionFailureKind::NeedsReconcile,
                            "stage_chunk_length_mismatch",
                            None,
                        ));
                    }
                    match self.drive.upload_chunk(&mut session, &bytes) {
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

        self.finish_upload(intent, expected_local_revision, &stage, &remote)
    }

    fn finish_upload(
        &self,
        intent: &TransferIntent,
        local_revision: &str,
        stage: &TransferStageRef,
        remote: &RemoteObject,
    ) -> Result<VerifiedTransfer, ExecutionFailure> {
        if remote.sha256() != intent.sha256_hex() || remote.size() != intent.byte_len() {
            return Err(failure(
                ExecutionFailureKind::NeedsReconcile,
                "remote_evidence_mismatch",
                None,
            ));
        }
        let base = self
            .service
            .publish_verified_stage_as_base(self.session_id, stage, MAX_TRANSFER_BYTES_USIZE)
            .map_err(local_failure)?;
        VerifiedTransfer::new(
            intent.operation_id(),
            remote.file_id(),
            remote.sync_revision(),
            Some(local_revision.to_owned()),
            intent.sha256_hex(),
            intent.byte_len(),
            base.opaque_ref(),
            "upload_verified",
        )
        .map_err(|_| {
            failure(
                ExecutionFailureKind::NeedsReconcile,
                "verified_evidence_invalid",
                None,
            )
        })
    }

    fn execute_download(
        &mut self,
        intent: &TransferIntent,
    ) -> Result<VerifiedTransfer, ExecutionFailure> {
        let expected_stage = format!("stage-{}", intent.operation_id());
        if intent.stage_ref() != Some(expected_stage.as_str()) {
            return Err(failure(
                ExecutionFailureKind::NeedsReconcile,
                "stage_reference_mismatch",
                None,
            ));
        }
        let remote_file_id = intent.remote_file_id().ok_or_else(|| {
            failure(
                ExecutionFailureKind::NeedsReconcile,
                "remote_file_missing",
                None,
            )
        })?;
        let remote_revision = intent.expected_remote_revision().ok_or_else(|| {
            failure(
                ExecutionFailureKind::NeedsReconcile,
                "remote_revision_missing",
                None,
            )
        })?;
        let download = DownloadIntent::from_sync_revision(
            remote_file_id,
            intent.parent_id(),
            remote_revision,
            intent.sha256_hex(),
            intent.byte_len(),
        )
        .map_err(|error| drive_failure(error, false))?;

        let stage = match self.service.load_transfer_stage(
            self.session_id,
            intent.operation_id(),
            intent.sha256_hex(),
            intent.byte_len(),
            MAX_TRANSFER_BYTES_USIZE,
        ) {
            Ok(stage) => {
                self.drive
                    .verify_download(&download)
                    .map_err(|error| drive_failure(error, false))?;
                stage
            }
            Err(NativeTransferError::StageUnavailable) => {
                self.download_into_new_stage(intent, &download)?
            }
            Err(NativeTransferError::DigestMismatch) => {
                // The durable operation identity authorizes removal of only
                // its own proven-incomplete private stage. Exact verified or
                // hardlinked evidence is refused by the app-service boundary.
                self.service
                    .discard_incomplete_transfer_stage(
                        self.session_id,
                        intent.operation_id(),
                        intent.sha256_hex(),
                        intent.byte_len(),
                        MAX_TRANSFER_BYTES_USIZE,
                    )
                    .map_err(local_failure)?;
                self.download_into_new_stage(intent, &download)?
            }
            Err(error) => return Err(local_failure(error)),
        };
        validate_stage(intent, &stage)?;

        match self.service.publish_staged_transfer(
            self.session_id,
            intent.path(),
            &stage,
            None,
            MAX_TRANSFER_BYTES_USIZE,
        ) {
            Ok(publication) => VerifiedTransfer::new(
                intent.operation_id(),
                remote_file_id,
                remote_revision,
                Some(publication.snapshot.revision.hex),
                intent.sha256_hex(),
                intent.byte_len(),
                publication.base_ref.opaque_ref(),
                "download_created_verified",
            )
            .map_err(|_| {
                failure(
                    ExecutionFailureKind::NeedsReconcile,
                    "verified_evidence_invalid",
                    None,
                )
            }),
            Err(NativeTransferError::StaleRevision | NativeTransferError::UnsupportedReplace) => {
                // Base publication happens before the create-no-replace call.
                // If a previous attempt already created the exact local file,
                // prove its bytes again and complete idempotently.
                let mut sink = std::io::sink();
                let local = self
                    .service
                    .stream_transfer_source(
                        self.session_id,
                        intent.path(),
                        &mut sink,
                        MAX_TRANSFER_BYTES_USIZE,
                    )
                    .map_err(local_failure)?;
                if local.sha256.as_str() != intent.sha256_hex()
                    || local.byte_len != intent.byte_len()
                {
                    return Err(failure(
                        ExecutionFailureKind::NeedsReconcile,
                        "local_create_conflict",
                        None,
                    ));
                }
                VerifiedTransfer::new(
                    intent.operation_id(),
                    remote_file_id,
                    remote_revision,
                    Some(local.revision.hex),
                    intent.sha256_hex(),
                    intent.byte_len(),
                    format!("sha256-{}", intent.sha256_hex()),
                    "download_existing_verified",
                )
                .map_err(|_| {
                    failure(
                        ExecutionFailureKind::NeedsReconcile,
                        "verified_evidence_invalid",
                        None,
                    )
                })
            }
            Err(error) => Err(local_failure(error)),
        }
    }

    fn download_into_new_stage(
        &mut self,
        intent: &TransferIntent,
        download: &DownloadIntent,
    ) -> Result<TransferStageRef, ExecutionFailure> {
        let mut writer = self
            .service
            .begin_transfer_stage(
                self.session_id,
                intent.operation_id(),
                MAX_TRANSFER_BYTES_USIZE,
            )
            .map_err(local_failure)?;
        self.drive
            .download_blob_to(download, &mut writer)
            .map_err(download_stream_failure)?;
        self.service
            .finish_transfer_stage(
                self.session_id,
                writer,
                intent.sha256_hex(),
                intent.byte_len(),
                MAX_TRANSFER_BYTES_USIZE,
            )
            .map_err(local_failure)
    }
}

impl TransferExecutor for NativeTransferExecutor<'_> {
    fn execute(&mut self, intent: &TransferIntent) -> Result<VerifiedTransfer, ExecutionFailure> {
        self.service
            .confirm_active_session(self.session_id)
            .map_err(|_| {
                failure(
                    ExecutionFailureKind::NeedsReconcile,
                    "vault_session_changed",
                    None,
                )
            })?;
        match intent.direction() {
            TransferDirection::Upload => self.execute_upload(intent),
            TransferDirection::Download => self.execute_download(intent),
        }
    }
}

fn validate_stage(
    intent: &TransferIntent,
    stage: &TransferStageRef,
) -> Result<(), ExecutionFailure> {
    if stage.operation_id() != intent.operation_id()
        || stage.snapshot().sha256.as_str() != intent.sha256_hex()
        || stage.snapshot().byte_len != intent.byte_len()
    {
        Err(failure(
            ExecutionFailureKind::NeedsReconcile,
            "stage_evidence_mismatch",
            None,
        ))
    } else {
        Ok(())
    }
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
        DriveErrorCode::Unauthorized => ExecutionFailureKind::AuthRequired,
        DriveErrorCode::RateLimited => ExecutionFailureKind::RateLimited,
        // A redacted transport failure is the only signal currently available
        // for a disconnected network. Treat it as Offline only while replaying
        // the exact durable intent is proven side-effect-free. Timeouts and 5xx
        // responses may also be transient, but do not prove that the device is
        // offline and therefore use the ordinary safe retry schedule.
        DriveErrorCode::Transport if !side_effect_possible => ExecutionFailureKind::Offline,
        code if is_unknown_transport(code) && side_effect_possible => {
            ExecutionFailureKind::TransientUnknown
        }
        code if is_unknown_transport(code) => ExecutionFailureKind::TransientSafe,
        _ => ExecutionFailureKind::NeedsReconcile,
    }
}

fn local_failure(error: NativeTransferError) -> ExecutionFailure {
    let code = match error {
        NativeTransferError::InvalidRequest => "local_invalid_request",
        NativeTransferError::ProtectedPath => "local_protected_path",
        NativeTransferError::StaleRevision => "local_stale_revision",
        NativeTransferError::UnsupportedReplace => "local_replace_unsupported",
        NativeTransferError::DigestMismatch => "local_digest_mismatch",
        NativeTransferError::ResourceLimit => "local_resource_limit",
        NativeTransferError::StageUnavailable => "local_stage_unavailable",
        NativeTransferError::StageAlreadyExists => "local_stage_collision",
        NativeTransferError::PrivateStoreUnavailable => "local_private_store_unavailable",
        NativeTransferError::PublicationUnknown => "local_publication_unknown",
        NativeTransferError::VaultUnavailable => "local_vault_unavailable",
    };
    failure(ExecutionFailureKind::NeedsReconcile, code, None)
}

fn download_stream_failure(error: DriveError) -> ExecutionFailure {
    let kind = classify_download_stream_failure(error.code());
    failure(kind, error.code().as_str(), None)
}

fn classify_download_stream_failure(code: DriveErrorCode) -> ExecutionFailureKind {
    if code == DriveErrorCode::Unauthorized {
        ExecutionFailureKind::AuthRequired
    } else {
        // Once a private stage exists, interrupted or rejected streaming can
        // leave partial evidence. R2 preserves it for explicit reconciliation
        // instead of truncating it for an automatic retry, including when the
        // underlying failure looks like a disconnected network.
        ExecutionFailureKind::NeedsReconcile
    }
}

fn failure(
    kind: ExecutionFailureKind,
    code: &'static str,
    retry_after: Option<Duration>,
) -> ExecutionFailure {
    ExecutionFailure::new(kind, code, retry_after)
        .expect("native transfer classifications are compile-time bounded")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use myvault_core::Vault;
    use std::{fs, io::Write};
    use uuid::Uuid;

    #[test]
    fn stage_reference_is_operation_scoped() {
        let id = Uuid::new_v4();
        assert_eq!(format!("stage-{id}"), format!("stage-{}", id));
        assert!(!format!("stage-{id}").contains('/'));
    }

    #[test]
    fn local_failure_codes_are_redacted_and_stable() {
        for error in [
            NativeTransferError::ProtectedPath,
            NativeTransferError::DigestMismatch,
            NativeTransferError::PublicationUnknown,
        ] {
            let mapped = local_failure(error);
            assert_eq!(mapped.kind(), ExecutionFailureKind::NeedsReconcile);
            assert!(!mapped.code().contains('/'));
            assert!(!mapped.code().contains("Bearer"));
        }
    }

    #[test]
    fn offline_is_reserved_for_pre_side_effect_transport_failure() {
        assert_eq!(
            classify_drive_failure(DriveErrorCode::Transport, false),
            ExecutionFailureKind::Offline
        );
        assert_eq!(
            classify_drive_failure(DriveErrorCode::Timeout, false),
            ExecutionFailureKind::TransientSafe
        );
        assert_eq!(
            classify_drive_failure(DriveErrorCode::TransientProvider, false),
            ExecutionFailureKind::TransientSafe
        );
    }

    #[test]
    fn upload_transport_failures_after_possible_mutation_never_pause_offline() {
        for code in [
            DriveErrorCode::Transport,
            DriveErrorCode::Timeout,
            DriveErrorCode::TransientProvider,
        ] {
            assert_eq!(
                classify_drive_failure(code, true),
                ExecutionFailureKind::TransientUnknown
            );
        }
    }

    #[test]
    fn partial_download_stage_never_pauses_offline() {
        for code in [
            DriveErrorCode::Transport,
            DriveErrorCode::Timeout,
            DriveErrorCode::TransientProvider,
        ] {
            assert_eq!(
                classify_download_stream_failure(code),
                ExecutionFailureKind::NeedsReconcile
            );
        }
        assert_eq!(
            classify_download_stream_failure(DriveErrorCode::Unauthorized),
            ExecutionFailureKind::AuthRequired
        );
    }

    #[test]
    fn upload_existing_exact_bytes_completes_without_remote_mutation() {
        let temporary = tempfile::tempdir().expect("temporary roots");
        let base = temporary
            .path()
            .canonicalize()
            .expect("canonical temporary root");
        let app_data = base.join("app-data");
        let vault_root = base.join("vault");
        fs::create_dir(&app_data).expect("app data");
        fs::create_dir(&vault_root).expect("vault root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&app_data, fs::Permissions::from_mode(0o700))
                .expect("private app data");
        }
        fs::write(vault_root.join("note.md"), b"abc").expect("source");
        let service = AppService::with_app_data_root(&app_data);
        let session_id = service
            .activate_trusted_vault(Vault::open(&vault_root).expect("open vault"))
            .expect("activate")
            .session_id
            .expect("session");
        let operation_id = Uuid::new_v4();
        let mut sink = std::io::sink();
        let snapshot = service
            .stream_transfer_source(session_id, "note.md", &mut sink, MAX_TRANSFER_BYTES_USIZE)
            .expect("snapshot");
        let marker = format!("r2-{}", operation_id.simple());
        let intent = TransferIntent::new(
            operation_id,
            TransferDirection::Upload,
            "note.md",
            "root_1",
            None,
            Some(snapshot.revision.hex.clone()),
            None,
            snapshot.sha256.as_str(),
            snapshot.byte_len,
            ContentKind::Markdown,
            marker.clone(),
            Some(format!("stage-{operation_id}")),
            None,
            0,
        )
        .expect("intent");

        let mut server = Server::new();
        server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
            .expect(2)
            .create();
        server
            .mock("GET", "/drive/v3/files/root_1")
            .match_query(Matcher::Any)
            .with_body(r#"{"id":"root_1","name":"Root","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#)
            .expect(2)
            .create();
        let remote = format!(
            r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{}","appProperties":{{"myvaultOperation":"{marker}","myvaultSha256":"{}","myvaultSize":"3"}}}}"#,
            snapshot.sha256.as_str(),
            snapshot.sha256.as_str(),
        );
        server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Any)
            .with_body(format!(r#"{{"files":[{remote}]}}"#))
            .expect(1)
            .create();
        server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Any)
            .with_body(remote)
            .expect(1)
            .create();
        let post = server
            .mock("POST", "/upload/drive/v3/files")
            .match_query(Matcher::Any)
            .expect(0)
            .create();
        let origin = server.url();
        let drive = TransferDrive::for_test_origins(
            &format!("{origin}/drive/v3/"),
            &format!("{origin}/upload/drive/v3/"),
            "account_1",
            "root_1",
            MAX_TRANSFER_BYTES,
        )
        .expect("test Drive");
        let mut executor = NativeTransferExecutor::new(&service, session_id, drive);

        let verified = executor.execute(&intent).expect("verified no-op");

        assert_eq!(verified.remote_file_id(), "file_1");
        assert_eq!(verified.sha256_hex(), snapshot.sha256.as_str());
        assert_eq!(
            verified.local_revision(),
            Some(snapshot.revision.hex.as_str())
        );
        assert_eq!(verified.outcome_code(), "upload_verified");
        post.assert();
    }

    #[test]
    fn created_upload_completes_only_after_post_create_uniqueness_proof() {
        let temporary = tempfile::tempdir().expect("temporary roots");
        let base = temporary
            .path()
            .canonicalize()
            .expect("canonical temporary root");
        let app_data = base.join("app-data");
        let vault_root = base.join("vault");
        fs::create_dir(&app_data).expect("app data");
        fs::create_dir(&vault_root).expect("vault root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&app_data, fs::Permissions::from_mode(0o700))
                .expect("private app data");
        }
        fs::write(vault_root.join("note.md"), b"abc").expect("source");
        let service = AppService::with_app_data_root(&app_data);
        let session_id = service
            .activate_trusted_vault(Vault::open(&vault_root).expect("open vault"))
            .expect("activate")
            .session_id
            .expect("session");
        let operation_id = Uuid::new_v4();
        let mut sink = std::io::sink();
        let snapshot = service
            .stream_transfer_source(session_id, "note.md", &mut sink, MAX_TRANSFER_BYTES_USIZE)
            .expect("snapshot");
        let marker = format!("r2-{}", operation_id.simple());
        let intent = TransferIntent::new(
            operation_id,
            TransferDirection::Upload,
            "note.md",
            "root_1",
            None,
            Some(snapshot.revision.hex.clone()),
            None,
            snapshot.sha256.as_str(),
            snapshot.byte_len,
            ContentKind::Markdown,
            marker.clone(),
            Some(format!("stage-{operation_id}")),
            None,
            0,
        )
        .expect("intent");

        let mut server = Server::new();
        server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
            .expect(8)
            .create();
        server
            .mock("GET", "/drive/v3/files/root_1")
            .match_query(Matcher::Any)
            .with_body(r#"{"id":"root_1","name":"Root","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#)
            .expect(8)
            .create();
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
        let remote = format!(
            r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{}","appProperties":{{"myvaultOperation":"{marker}","myvaultSha256":"{}","myvaultSize":"3"}}}}"#,
            snapshot.sha256.as_str(),
            snapshot.sha256.as_str(),
        );
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
        let drive = TransferDrive::for_test_origins(
            &format!("{origin}/drive/v3/"),
            &format!("{origin}/upload/drive/v3/"),
            "account_1",
            "root_1",
            MAX_TRANSFER_BYTES,
        )
        .expect("test Drive");
        let mut executor = NativeTransferExecutor::new(&service, session_id, drive);

        let verified = executor.execute(&intent).expect("verified create");

        assert_eq!(verified.remote_file_id(), "file_1");
        assert_eq!(verified.sha256_hex(), snapshot.sha256.as_str());
        assert_eq!(verified.outcome_code(), "upload_verified");
        initial_marker.assert();
        initial_name.assert();
        initiate.assert();
        upload.assert();
        exact_metadata.assert();
        final_marker.assert();
        final_name.assert();
    }

    #[test]
    fn download_exact_blob_creates_local_file_and_private_base() {
        let temporary = tempfile::tempdir().expect("temporary roots");
        let base = temporary
            .path()
            .canonicalize()
            .expect("canonical temporary root");
        let app_data = base.join("app-data");
        let vault_root = base.join("vault");
        fs::create_dir(&app_data).expect("app data");
        fs::create_dir(&vault_root).expect("vault root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&app_data, fs::Permissions::from_mode(0o700))
                .expect("private app data");
        }
        let service = AppService::with_app_data_root(&app_data);
        let session_id = service
            .activate_trusted_vault(Vault::open(&vault_root).expect("open vault"))
            .expect("activate")
            .session_id
            .expect("session");
        let operation_id = Uuid::new_v4();
        let sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let revision = "0000000000000000000000000000000000000000000000000000000000000002";
        let intent = TransferIntent::new(
            operation_id,
            TransferDirection::Download,
            "ภาษาไทย.md",
            "root_1",
            Some("file_1".to_owned()),
            None,
            Some(revision.to_owned()),
            sha256,
            3,
            ContentKind::Markdown,
            format!("r2-{}", operation_id.simple()),
            Some(format!("stage-{operation_id}")),
            None,
            0,
        )
        .expect("intent");

        let mut server = Server::new();
        server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
            .expect(2)
            .create();
        server
            .mock("GET", "/drive/v3/files/root_1")
            .match_query(Matcher::Any)
            .with_body(r#"{"id":"root_1","name":"Root","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#)
            .expect(2)
            .create();
        let remote = format!(
            r#"{{"id":"file_1","name":"ภาษาไทย.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{sha256}"}}"#,
        );
        server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(remote)
            .expect(2)
            .create();
        let media = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("alt=media".into()))
            .with_header("content-length", "3")
            .with_body("abc")
            .expect(1)
            .create();
        let origin = server.url();
        let drive = TransferDrive::for_test_origins(
            &format!("{origin}/drive/v3/"),
            &format!("{origin}/upload/drive/v3/"),
            "account_1",
            "root_1",
            MAX_TRANSFER_BYTES,
        )
        .expect("test Drive");
        let mut executor = NativeTransferExecutor::new(&service, session_id, drive);

        let verified = executor.execute(&intent).expect("verified download");

        assert_eq!(fs::read(vault_root.join("ภาษาไทย.md")).unwrap(), b"abc");
        assert_eq!(verified.remote_file_id(), "file_1");
        assert_eq!(verified.remote_revision(), revision);
        assert_eq!(verified.base_ref(), format!("sha256-{sha256}"));
        assert_eq!(verified.outcome_code(), "download_created_verified");
        media.assert();
    }

    #[test]
    fn restarted_download_discards_partial_stage_before_safe_replay() {
        let temporary = tempfile::tempdir().expect("temporary roots");
        let base = temporary
            .path()
            .canonicalize()
            .expect("canonical temporary root");
        let app_data = base.join("app-data");
        let vault_root = base.join("vault");
        fs::create_dir(&app_data).expect("app data");
        fs::create_dir(&vault_root).expect("vault root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&app_data, fs::Permissions::from_mode(0o700))
                .expect("private app data");
        }
        let operation_id = Uuid::new_v4();
        let interrupted = AppService::with_app_data_root(&app_data);
        let interrupted_session = interrupted
            .activate_trusted_vault(Vault::open(&vault_root).expect("open vault"))
            .expect("activate")
            .session_id
            .expect("session");
        let mut partial = interrupted
            .begin_transfer_stage(interrupted_session, operation_id, MAX_TRANSFER_BYTES_USIZE)
            .expect("partial stage");
        partial.write_all(b"a").expect("partial byte");
        drop(partial);
        drop(interrupted);

        let service = AppService::with_app_data_root(&app_data);
        let session_id = service
            .activate_trusted_vault(Vault::open(&vault_root).expect("reopen vault"))
            .expect("reactivate")
            .session_id
            .expect("new session");
        let sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let revision = "0000000000000000000000000000000000000000000000000000000000000002";
        let intent = TransferIntent::new(
            operation_id,
            TransferDirection::Download,
            "recovered.bin",
            "root_1",
            Some("file_1".to_owned()),
            None,
            Some(revision.to_owned()),
            sha256,
            3,
            ContentKind::Blob,
            format!("r2-{}", operation_id.simple()),
            Some(format!("stage-{operation_id}")),
            None,
            1,
        )
        .expect("intent");

        let mut server = Server::new();
        server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
            .expect(2)
            .create();
        server
            .mock("GET", "/drive/v3/files/root_1")
            .match_query(Matcher::Any)
            .with_body(r#"{"id":"root_1","name":"Root","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#)
            .expect(2)
            .create();
        let remote = format!(
            r#"{{"id":"file_1","name":"recovered.bin","mimeType":"application/octet-stream","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{sha256}"}}"#,
        );
        server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(remote)
            .expect(2)
            .create();
        let media = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("alt=media".into()))
            .with_header("content-length", "3")
            .with_body("abc")
            .expect(1)
            .create();
        let origin = server.url();
        let drive = TransferDrive::for_test_origins(
            &format!("{origin}/drive/v3/"),
            &format!("{origin}/upload/drive/v3/"),
            "account_1",
            "root_1",
            MAX_TRANSFER_BYTES,
        )
        .expect("test Drive");
        let mut executor = NativeTransferExecutor::new(&service, session_id, drive);

        let verified = executor.execute(&intent).expect("recovered download");

        assert_eq!(fs::read(vault_root.join("recovered.bin")).unwrap(), b"abc");
        assert_eq!(verified.remote_file_id(), "file_1");
        assert_eq!(verified.remote_revision(), revision);
        assert_eq!(verified.base_ref(), format!("sha256-{sha256}"));
        assert_eq!(verified.outcome_code(), "download_created_verified");
        media.assert();
    }
}
