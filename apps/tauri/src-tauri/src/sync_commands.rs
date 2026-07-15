#![cfg_attr(target_os = "android", allow(dead_code))]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use myvault_app_service::{AppError, AppErrorCode, VaultSessionId};
use myvault_drive::{ErrorCode as DriveErrorCode, FilePage};
use myvault_sync_engine::{
    InitialSyncProgress, RemoteEntryKind, RemotePreviewCursor, RemotePreviewPage, SyncPhase,
    TransferSummary,
};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_PREVIEW_LIMIT: usize = 100;
const MAX_CURSOR_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncCommandCode {
    NoActiveVault,
    StaleSession,
    InvalidRequest,
    Unsupported,
    Unconfigured,
    AuthRequired,
    BindingMismatch,
    RescanRequired,
    Busy,
    ProviderUnavailable,
    StorageUnavailable,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCommandError {
    pub code: SyncCommandCode,
    pub message: &'static str,
}

impl SyncCommandError {
    const fn new(code: SyncCommandCode, message: &'static str) -> Self {
        Self { code, message }
    }

    const fn unsupported() -> Self {
        Self::new(
            SyncCommandCode::Unsupported,
            "read-only Drive metadata sync is unavailable on this platform",
        )
    }

    const fn internal() -> Self {
        Self::new(
            SyncCommandCode::Internal,
            "the native sync service is unavailable",
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusDto {
    pub session_id: VaultSessionId,
    pub supported: bool,
    pub binding_available: bool,
    pub configured: bool,
    pub connected: bool,
    pub bound: bool,
    pub account_id: Option<String>,
    pub root_id: Option<String>,
    pub root_name: Option<String>,
    pub phase: &'static str,
    pub rescan_required: bool,
    pub active: u64,
    pub pending: u64,
    pub retry_scheduled: u64,
    pub auth_required: u64,
    pub needs_reconcile: u64,
    pub completed: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFolderDto {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFolderPageDto {
    pub session_id: VaultSessionId,
    pub parent_id: Option<String>,
    pub folders: Vec<RemoteFolderDto>,
    pub next_page_token: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindRootDto {
    pub session_id: VaultSessionId,
    pub outcome: &'static str,
    pub status: SyncStatusDto,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStepDto {
    pub session_id: VaultSessionId,
    pub progress: &'static str,
    pub status: SyncStatusDto,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePreviewEntryDto {
    pub file_id: String,
    pub path: String,
    pub kind: &'static str,
    pub path_collision: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePreviewPageDto {
    pub session_id: VaultSessionId,
    pub entries: Vec<RemotePreviewEntryDto>,
    pub next_after: Option<String>,
    pub has_more: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewCursorWire {
    path: String,
    file_id: String,
}

fn parse_session_id(value: &str) -> Result<VaultSessionId, SyncCommandError> {
    VaultSessionId::parse(value).map_err(map_app_error)
}

fn map_app_error(error: AppError) -> SyncCommandError {
    match error.code {
        AppErrorCode::NoActiveSession => SyncCommandError::new(
            SyncCommandCode::NoActiveVault,
            "no local vault session is active",
        ),
        AppErrorCode::StaleSession => SyncCommandError::new(
            SyncCommandCode::StaleSession,
            "the local vault session is stale",
        ),
        AppErrorCode::InvalidSessionId => SyncCommandError::new(
            SyncCommandCode::InvalidRequest,
            "the vault session identifier is invalid",
        ),
        AppErrorCode::UnsupportedPlatform => SyncCommandError::unsupported(),
        _ => SyncCommandError::internal(),
    }
}

fn map_drive_error(error: myvault_drive::Error) -> SyncCommandError {
    match error.code() {
        DriveErrorCode::Unauthorized => SyncCommandError::new(
            SyncCommandCode::AuthRequired,
            "Google authorization is required",
        ),
        DriveErrorCode::InvalidAccount | DriveErrorCode::InvalidRoot => SyncCommandError::new(
            SyncCommandCode::BindingMismatch,
            "the Google account or exact Drive root does not match",
        ),
        DriveErrorCode::InvalidInput => SyncCommandError::new(
            SyncCommandCode::InvalidRequest,
            "a Drive metadata request identifier or cursor is invalid",
        ),
        DriveErrorCode::CursorExpired | DriveErrorCode::CursorAmbiguous => SyncCommandError::new(
            SyncCommandCode::RescanRequired,
            "the Drive metadata cursor requires a full rescan",
        ),
        _ => SyncCommandError::new(
            SyncCommandCode::ProviderUnavailable,
            "Google Drive metadata is temporarily unavailable",
        ),
    }
}

fn map_drive_with_rescan<T>(
    store: &mut myvault_sync_engine::SyncStore,
    result: myvault_drive::Result<T>,
) -> Result<T, SyncCommandError> {
    match result {
        Ok(value) => Ok(value),
        Err(error)
            if matches!(
                error.code(),
                DriveErrorCode::CursorExpired | DriveErrorCode::CursorAmbiguous
            ) =>
        {
            let mapped = map_drive_error(error);
            store
                .mark_rescan_required(now_unix_ms()?)
                .map_err(map_sync_error)?;
            Err(mapped)
        }
        Err(error) => Err(map_drive_error(error)),
    }
}

fn map_sync_error(error: myvault_sync_engine::Error) -> SyncCommandError {
    use myvault_sync_engine::Error;
    match error {
        Error::BindingCollision
        | Error::BindingIdentityMismatch
        | Error::BindingRequiresAccount => SyncCommandError::new(
            SyncCommandCode::BindingMismatch,
            "the Google account or exact Drive root does not match",
        ),
        Error::RescanRequired => SyncCommandError::new(
            SyncCommandCode::RescanRequired,
            "the Drive metadata cursor requires a full rescan",
        ),
        Error::InvalidRemoteId
        | Error::InvalidRemoteToken
        | Error::InvalidPreviewCursor
        | Error::InvalidPreviewLimit => SyncCommandError::new(
            SyncCommandCode::InvalidRequest,
            "the sync request identifier, cursor, or limit is invalid",
        ),
        Error::Remote(remote)
            if matches!(remote.code(), "drive_unauthorized" | "drive_forbidden") =>
        {
            SyncCommandError::new(
                SyncCommandCode::AuthRequired,
                "Google authorization is required",
            )
        }
        Error::Remote(remote) if matches!(remote.code(), "cursor_expired" | "cursor_ambiguous") => {
            SyncCommandError::new(
                SyncCommandCode::RescanRequired,
                "the Drive metadata cursor requires a full rescan",
            )
        }
        Error::Remote(remote) if remote.code() == "stale_session" => SyncCommandError::new(
            SyncCommandCode::StaleSession,
            "the local vault session is stale",
        ),
        Error::Remote(_) => SyncCommandError::new(
            SyncCommandCode::ProviderUnavailable,
            "Google Drive metadata is temporarily unavailable",
        ),
        _ => SyncCommandError::new(
            SyncCommandCode::StorageUnavailable,
            "private sync state is unavailable",
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn requeue_prior_absent_upload_if_exact(
    store: &mut myvault_sync_engine::SyncStore,
    operation_id: uuid::Uuid,
    portable_path: &str,
    remote_parent_id: &str,
    local_revision: &str,
    sha256: &str,
    byte_length: u64,
    mime_class: myvault_sync_engine::TransferMimeClass,
    reconciliation_time: u64,
    excluded_error_code: Option<&str>,
) -> Result<bool, SyncCommandError> {
    let Some(prior) = store.transfer(operation_id).map_err(map_sync_error)? else {
        return Ok(false);
    };
    if prior.phase != myvault_sync_engine::TransferPhase::NeedsReconcile
        || excluded_error_code
            .is_some_and(|excluded| prior.last_error_code.as_deref() == Some(excluded))
    {
        return Ok(false);
    }
    let expected_marker = format!("r2-{}", operation_id.simple());
    let expected_stage = format!("stage-{operation_id}");
    let exact_prior_absent_upload = prior.direction
        == myvault_sync_engine::TransferDirection::Upload
        && prior.portable_path == portable_path
        && prior.remote_parent_id == remote_parent_id
        && prior.remote_file_id.is_none()
        && prior.expected_local_revision.as_deref() == Some(local_revision)
        && prior.expected_remote_revision.is_none()
        && prior.sha256 == sha256
        && prior.byte_length == byte_length
        && prior.mime_class == mime_class
        && prior.operation_marker == expected_marker
        && prior.stage_reference.as_deref() == Some(expected_stage.as_str());
    if !exact_prior_absent_upload {
        return Err(SyncCommandError::internal());
    }
    store
        .requeue_transfer_for_reconciliation(
            operation_id,
            reconciliation_time.max(prior.updated_at_unix_ms),
        )
        .map_err(map_sync_error)?;
    Ok(true)
}

fn now_unix_ms() -> Result<u64, SyncCommandError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SyncCommandError::internal())?;
    u64::try_from(duration.as_millis()).map_err(|_| SyncCommandError::internal())
}

const fn phase_name(phase: SyncPhase) -> &'static str {
    match phase {
        SyncPhase::NeedStartToken => "needStartToken",
        SyncPhase::Scanning => "scanning",
        SyncPhase::Draining => "draining",
        SyncPhase::Ready => "ready",
    }
}

const fn progress_name(progress: InitialSyncProgress) -> &'static str {
    match progress {
        InitialSyncProgress::StartTokenCaptured => "startTokenCaptured",
        InitialSyncProgress::ScanPageCommitted => "scanPageCommitted",
        InitialSyncProgress::ScanComplete => "scanComplete",
        InitialSyncProgress::ChangesPageCommitted => "changesPageCommitted",
        InitialSyncProgress::Ready => "ready",
    }
}

fn folder_page(
    session_id: VaultSessionId,
    parent_id: Option<String>,
    page: FilePage,
) -> RemoteFolderPageDto {
    let folders = page
        .files
        .into_iter()
        .filter(|file| file.is_folder() && !file.trashed)
        .map(|file| RemoteFolderDto {
            id: file.id,
            name: file.name,
        })
        .collect();
    RemoteFolderPageDto {
        session_id,
        parent_id,
        folders,
        next_page_token: page.next_page_token,
    }
}

fn decode_preview_cursor(
    value: Option<&str>,
) -> Result<Option<RemotePreviewCursor>, SyncCommandError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() || value.len() > MAX_CURSOR_BYTES * 2 {
        return Err(SyncCommandError::new(
            SyncCommandCode::InvalidRequest,
            "the preview cursor is invalid",
        ));
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| {
        SyncCommandError::new(
            SyncCommandCode::InvalidRequest,
            "the preview cursor is invalid",
        )
    })?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(SyncCommandError::new(
            SyncCommandCode::InvalidRequest,
            "the preview cursor is invalid",
        ));
    }
    let cursor: PreviewCursorWire = serde_json::from_slice(&bytes).map_err(|_| {
        SyncCommandError::new(
            SyncCommandCode::InvalidRequest,
            "the preview cursor is invalid",
        )
    })?;
    Ok(Some(RemotePreviewCursor {
        path: cursor.path,
        file_id: cursor.file_id,
    }))
}

fn encode_preview_cursor(cursor: &RemotePreviewCursor) -> Result<String, SyncCommandError> {
    let bytes = serde_json::to_vec(&PreviewCursorWire {
        path: cursor.path.clone(),
        file_id: cursor.file_id.clone(),
    })
    .map_err(|_| SyncCommandError::internal())?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(SyncCommandError::internal());
    }
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn preview_page(
    session_id: VaultSessionId,
    page: RemotePreviewPage,
) -> Result<RemotePreviewPageDto, SyncCommandError> {
    Ok(RemotePreviewPageDto {
        session_id,
        entries: page
            .entries
            .into_iter()
            .map(|entry| RemotePreviewEntryDto {
                file_id: entry.file_id,
                path: entry.path,
                kind: match entry.kind {
                    RemoteEntryKind::File => "file",
                    RemoteEntryKind::Folder => "folder",
                },
                path_collision: entry.path_collision,
            })
            .collect(),
        next_after: page
            .next_after
            .as_ref()
            .map(encode_preview_cursor)
            .transpose()?,
        has_more: page.has_more,
    })
}

#[cfg(not(target_os = "android"))]
mod platform {
    use super::*;
    use crate::transfer_runtime::NativeTransferExecutor;
    use myvault_app_service::{AppService, ExplorerKindDto, NativeVaultContext};
    use myvault_desktop_auth::{
        DesktopOAuth, FreshAccessToken, GoogleClientSecret, GoogleTokenClient, NativeTokenProvider,
        OsKeyringStore, SecretStore,
    };
    use myvault_drive::{AccessToken, ReadOnlyDrive, ResolvedDriveChange, TransferDrive};
    use myvault_sync_engine::{
        advance_initial_sync, BindOutcome, RemoteChange, RemotePreviewEntry, SyncStore,
        TransferDirection, TransferMimeClass, TransferPhase, TransferRecord,
        TransferRegistrationOutcome, VaultSyncState,
    };
    use myvault_transfer::{WorkOutcome, Worker, MAX_TRANSFER_BYTES};
    use std::{
        collections::{BTreeMap, BTreeSet},
        process::{Command, Stdio},
        sync::{Arc, Mutex},
        time::Duration,
    };
    use unicode_normalization::UnicodeNormalization;
    use uuid::Uuid;

    const CLIENT_ID_ENV: &str = "MYVAULT_GOOGLE_DESKTOP_CLIENT_ID";
    const CLIENT_SECRET_ENV: &str = "MYVAULT_GOOGLE_DESKTOP_CLIENT_SECRET";
    // R2 deliberately uses a new credential namespace. A refresh token granted
    // for R1's metadata-only scope must never make the full-Drive runtime look
    // connected without an explicit consent upgrade.
    const KEYRING_SERVICE: &str = "com.abhuri.myvault.google-drive.r2-full-drive";
    const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
    const TRANSFER_PAGE_SIZE: usize = 200;
    const MAX_GUARDED_OPERATIONS: usize = 1_000;
    const MAX_INCREMENTAL_PAGES: usize = 100;
    const TRANSFER_NAMESPACE: Uuid = Uuid::from_u128(0xa9c0_4bb5_7db8_5a83_8e1c_d67a_a646_a4f2);

    #[derive(Default)]
    pub struct SyncRuntime {
        inner: Mutex<RuntimeInner>,
    }

    #[derive(Default)]
    struct RuntimeInner {
        connected_account_id: Option<String>,
        root_name: Option<String>,
        active: Option<ActiveSync>,
        transfer_running_session: Option<VaultSessionId>,
    }

    struct ActiveSync {
        session_id: VaultSessionId,
        store: SyncStore,
    }

    struct DetachedActiveSync {
        runtime: Arc<SyncRuntime>,
        active: Option<ActiveSync>,
    }

    impl DetachedActiveSync {
        fn store_mut(&mut self) -> Result<&mut SyncStore, SyncCommandError> {
            self.active
                .as_mut()
                .map(|active| &mut active.store)
                .ok_or_else(SyncCommandError::internal)
        }

        fn restore(mut self) -> Result<(), SyncCommandError> {
            self.restore_inner()
        }

        fn discard_store(&mut self) -> Result<(), SyncCommandError> {
            let session_id = self
                .active
                .as_ref()
                .map(|active| active.session_id)
                .ok_or_else(SyncCommandError::internal)?;
            drop(self.active.take());
            let mut inner = self
                .runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if inner.active.is_some() || inner.transfer_running_session != Some(session_id) {
                return Err(SyncCommandError::internal());
            }
            inner.transfer_running_session = None;
            Ok(())
        }

        fn restore_inner(&mut self) -> Result<(), SyncCommandError> {
            let Some(active) = self.active.take() else {
                return Ok(());
            };
            let mut inner = self
                .runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if inner.active.is_some() || inner.transfer_running_session != Some(active.session_id) {
                return Err(SyncCommandError::internal());
            }
            inner.active = Some(active);
            inner.transfer_running_session = None;
            Ok(())
        }
    }

    impl Drop for DetachedActiveSync {
        fn drop(&mut self) {
            let _ = self.restore_inner();
        }
    }

    fn desktop_client_id() -> Result<String, SyncCommandError> {
        let value = std::env::var(CLIENT_ID_ENV).map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::Unconfigured,
                "desktop Google OAuth is not configured",
            )
        })?;
        validate_client_id(&value)?;
        Ok(value)
    }

    fn desktop_client_secret() -> Result<GoogleClientSecret, SyncCommandError> {
        let value = std::env::var(CLIENT_SECRET_ENV).map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::Unconfigured,
                "desktop Google OAuth is not configured",
            )
        })?;
        GoogleClientSecret::parse(value).map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::Unconfigured,
                "desktop Google OAuth is not configured",
            )
        })
    }

    fn validate_client_id(value: &str) -> Result<(), SyncCommandError> {
        if value.is_empty()
            || value.len() > 512
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(SyncCommandError::new(
                SyncCommandCode::Unconfigured,
                "desktop Google OAuth is not configured",
            ));
        }
        Ok(())
    }

    fn is_configured() -> bool {
        desktop_client_id().is_ok() && desktop_client_secret().is_ok()
    }

    fn provider(
        client_id: &str,
    ) -> Result<NativeTokenProvider<GoogleTokenClient, OsKeyringStore>, SyncCommandError> {
        let endpoint = GoogleTokenClient::new(desktop_client_secret()?)
            .map_err(|_| SyncCommandError::internal())?;
        NativeTokenProvider::new(client_id, endpoint, OsKeyringStore::new(KEYRING_SERVICE))
            .map_err(|_| SyncCommandError::internal())
    }

    fn launch_system_browser(url: &str) -> Result<(), SyncCommandError> {
        #[cfg(target_os = "macos")]
        let mut command = Command::new("open");
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = Command::new("rundll32.exe");
            command.arg("url.dll,FileProtocolHandler");
            command
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = Command::new("xdg-open");

        command
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| {
                SyncCommandError::new(
                    SyncCommandCode::ProviderUnavailable,
                    "the system browser could not be opened",
                )
            })?;
        Ok(())
    }

    fn ensure_store<'a>(
        inner: &'a mut RuntimeInner,
        context: &NativeVaultContext,
    ) -> Result<&'a mut SyncStore, SyncCommandError> {
        if inner.transfer_running_session.is_some() {
            return Err(SyncCommandError::new(
                SyncCommandCode::Busy,
                "a guarded transfer is already running",
            ));
        }
        let same_session = inner
            .active
            .as_ref()
            .is_some_and(|active| active.session_id == context.session_id());
        if !same_session {
            inner.active = None;
            inner.connected_account_id = None;
            inner.root_name = None;
            let app_data_root = context.app_data_root().ok_or_else(|| {
                SyncCommandError::new(
                    SyncCommandCode::StorageUnavailable,
                    "private sync state is unavailable",
                )
            })?;
            let store = SyncStore::open(app_data_root, context.vault_root(), context.vault_id())
                .map_err(map_sync_error)?;
            inner.active = Some(ActiveSync {
                session_id: context.session_id(),
                store,
            });
        }
        inner
            .active
            .as_mut()
            .map(|active| &mut active.store)
            .ok_or_else(SyncCommandError::internal)
    }

    fn status_from(
        session_id: VaultSessionId,
        inner: &RuntimeInner,
        state: Option<&VaultSyncState>,
        transfers: TransferSummary,
    ) -> SyncStatusDto {
        SyncStatusDto {
            session_id,
            supported: true,
            binding_available: true,
            configured: is_configured(),
            connected: inner.connected_account_id.is_some(),
            bound: state.is_some_and(|value| value.account_id.is_some()),
            account_id: inner
                .connected_account_id
                .clone()
                .or_else(|| state.and_then(|value| value.account_id.clone())),
            root_id: state.map(|value| value.remote_root_id.clone()),
            root_name: inner.root_name.clone(),
            phase: state.map_or("unbound", |value| phase_name(value.phase)),
            rescan_required: state.is_some_and(|value| value.rescan_required),
            active: transfers.active(),
            pending: transfers.pending,
            retry_scheduled: transfers.retry_scheduled,
            auth_required: transfers.auth_required,
            needs_reconcile: transfers.needs_reconcile,
            completed: transfers.completed,
        }
    }

    fn refresh_connected_state(
        inner: &mut RuntimeInner,
        state: Option<&VaultSyncState>,
    ) -> Result<(), SyncCommandError> {
        if inner.connected_account_id.is_none() {
            if let Some(account_id) = state.and_then(|value| value.account_id.as_deref()) {
                let token = OsKeyringStore::new(KEYRING_SERVICE)
                    .load_refresh_token(account_id)
                    .map_err(|_| {
                        SyncCommandError::new(
                            SyncCommandCode::StorageUnavailable,
                            "secure credential storage is unavailable",
                        )
                    })?;
                if token.is_some() {
                    inner.connected_account_id = Some(account_id.to_owned());
                }
            }
        }
        Ok(())
    }

    fn fresh_access(account_id: &str) -> Result<FreshAccessToken, SyncCommandError> {
        let client_id = desktop_client_id()?;
        provider(&client_id)?
            .fresh_access_token(account_id)
            .map_err(|_| {
                SyncCommandError::new(
                    SyncCommandCode::AuthRequired,
                    "Google authorization is required",
                )
            })
    }

    fn read_only_drive(access: &FreshAccessToken) -> Result<ReadOnlyDrive, SyncCommandError> {
        ReadOnlyDrive::google(AccessToken::new(access.expose_to_native().to_owned()))
            .map_err(map_drive_error)
    }

    fn transfer_drive(
        access: &FreshAccessToken,
        account_id: &str,
        root_id: &str,
    ) -> Result<TransferDrive, SyncCommandError> {
        TransferDrive::google(
            AccessToken::new(access.expose_to_native().to_owned()),
            account_id,
            root_id,
        )
        .map_err(map_drive_error)
    }

    fn fresh_drive(account_id: &str) -> Result<ReadOnlyDrive, SyncCommandError> {
        read_only_drive(&fresh_access(account_id)?)
    }

    fn refresh_run_access_once(
        account_id: &str,
        access: &mut FreshAccessToken,
        refresh_attempted: &mut bool,
    ) -> Result<bool, SyncCommandError> {
        if !claim_auth_refresh(refresh_attempted) {
            return Ok(false);
        }
        *access = fresh_access(account_id)?;
        Ok(true)
    }

    fn claim_auth_refresh(refresh_attempted: &mut bool) -> bool {
        if *refresh_attempted {
            false
        } else {
            *refresh_attempted = true;
            true
        }
    }

    fn transfer_operation_id(parts: &[&str]) -> Uuid {
        let mut evidence = String::from("myvault-r2\0");
        for part in parts {
            evidence.push_str(&part.len().to_string());
            evidence.push(':');
            evidence.push_str(part);
            evidence.push('\0');
        }
        Uuid::new_v5(&TRANSFER_NAMESPACE, evidence.as_bytes())
    }

    fn stage_reference(operation_id: Uuid) -> String {
        format!("stage-{operation_id}")
    }

    fn operation_marker(operation_id: Uuid) -> String {
        format!("r2-{}", operation_id.simple())
    }

    fn content_kind(path: &str, markdown: bool) -> TransferMimeClass {
        if markdown
            || path
                .rsplit_once('.')
                .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("md"))
        {
            TransferMimeClass::Markdown
        } else {
            TransferMimeClass::Blob
        }
    }

    fn parent_path(path: &str) -> Option<&str> {
        path.rsplit_once('/').map(|(parent, _)| parent)
    }

    fn display_name(path: &str) -> Result<&str, SyncCommandError> {
        path.rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "a transfer path is invalid",
                )
            })
    }

    fn collision_key(path: &str) -> String {
        path.nfc().flat_map(char::to_lowercase).collect()
    }

    fn reject_portable_path_collisions<'a>(
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), SyncCommandError> {
        let mut seen = BTreeMap::new();
        for path in paths {
            let key = collision_key(path);
            if seen
                .insert(key, path)
                .is_some_and(|existing| existing != path)
            {
                return Err(SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "case-folded or Unicode-normalized transfer paths collide",
                ));
            }
        }
        Ok(())
    }

    fn collect_remote_preview(
        store: &SyncStore,
    ) -> Result<Vec<RemotePreviewEntry>, SyncCommandError> {
        let mut cursor = None;
        let mut entries = Vec::new();
        loop {
            let page = store
                .remote_preview(cursor.as_ref(), TRANSFER_PAGE_SIZE)
                .map_err(map_sync_error)?;
            if page.rescan_required || page.colliding_entries != 0 {
                return Err(SyncCommandError::new(
                    SyncCommandCode::RescanRequired,
                    "Drive metadata must be collision-free and freshly scanned before transfer",
                ));
            }
            if entries.len().saturating_add(page.entries.len()) > MAX_GUARDED_OPERATIONS * 2 {
                return Err(SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "the guarded transfer plan exceeds the operation limit",
                ));
            }
            entries.extend(page.entries);
            if !page.has_more {
                break;
            }
            cursor = page.next_after;
            if cursor.is_none() {
                return Err(SyncCommandError::internal());
            }
        }
        Ok(entries)
    }

    fn collect_local_entries(
        service: &AppService,
        session_id: VaultSessionId,
    ) -> Result<Vec<myvault_app_service::ExplorerEntryDto>, SyncCommandError> {
        let mut after = None;
        let mut entries = Vec::new();
        loop {
            let page = service
                .list_explorer(session_id, after.as_deref(), TRANSFER_PAGE_SIZE)
                .map_err(map_app_error)?;
            if entries.len().saturating_add(page.entries.len()) > MAX_GUARDED_OPERATIONS {
                return Err(SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "the guarded transfer plan exceeds the operation limit",
                ));
            }
            entries.extend(page.entries);
            if !page.has_more {
                break;
            }
            after = page.next_after;
            if after.is_none() {
                return Err(SyncCommandError::internal());
            }
        }
        Ok(entries)
    }

    fn prepare_guarded_transfers(
        service: &AppService,
        session_id: VaultSessionId,
        store: &mut SyncStore,
        drive: &TransferDrive,
        root_id: &str,
    ) -> Result<u64, SyncCommandError> {
        let remote_entries = collect_remote_preview(store)?;
        let local_entries = collect_local_entries(service, session_id)?;
        reject_portable_path_collisions(
            remote_entries
                .iter()
                .map(|entry| entry.path.as_str())
                .chain(local_entries.iter().map(|entry| entry.path.as_str())),
        )?;
        let local_paths = local_entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<BTreeSet<_>>();
        let remote_by_path = remote_entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect::<BTreeMap<_, _>>();
        let remote_folders = remote_entries
            .iter()
            .filter(|entry| entry.kind == RemoteEntryKind::Folder)
            .map(|entry| (entry.path.as_str(), entry.file_id.as_str()))
            .collect::<BTreeMap<_, _>>();
        let mut registered = 0_u64;
        let reconciliation_time = now_unix_ms()?;

        fn register_or_reconcile(
            store: &mut SyncStore,
            record: &TransferRecord,
            reconciliation_time: u64,
        ) -> Result<bool, SyncCommandError> {
            match store.register_transfer(record).map_err(map_sync_error)? {
                TransferRegistrationOutcome::Registered => Ok(true),
                TransferRegistrationOutcome::AlreadyCompleted => Ok(false),
                TransferRegistrationOutcome::AlreadyPresent => {
                    let existing = store
                        .transfer(record.operation_id)
                        .map_err(map_sync_error)?
                        .ok_or_else(SyncCommandError::internal)?;
                    if existing.phase == TransferPhase::NeedsReconcile {
                        store
                            .requeue_transfer_for_reconciliation(
                                record.operation_id,
                                reconciliation_time.max(existing.updated_at_unix_ms),
                            )
                            .map_err(map_sync_error)?;
                    }
                    Ok(false)
                }
            }
        }

        for local in local_entries {
            if local.byte_len > MAX_TRANSFER_BYTES {
                return Err(SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "a local file exceeds the guarded transfer size limit",
                ));
            }
            let mut sink = std::io::sink();
            let snapshot = service
                .stream_transfer_source(
                    session_id,
                    &local.path,
                    &mut sink,
                    MAX_TRANSFER_BYTES as usize,
                )
                .map_err(|_| {
                    SyncCommandError::new(
                        SyncCommandCode::StorageUnavailable,
                        "exact local transfer evidence is unavailable",
                    )
                })?;
            let matching_remote = remote_by_path.get(local.path.as_str()).copied();
            let matching_durable = matching_remote
                .map(|entry| {
                    store
                        .remote_entry(&entry.file_id)
                        .map_err(map_sync_error)?
                        .ok_or_else(SyncCommandError::internal)
                })
                .transpose()?;
            let matching_base = matching_remote
                .map(|entry| store.remote_base(&entry.file_id).map_err(map_sync_error))
                .transpose()?
                .flatten();
            let exact_base_match = matching_durable.as_ref().is_some_and(|durable| {
                matching_base.as_ref().is_some_and(|base| {
                    base.local_revision == snapshot.revision.hex
                        && base.remote_revision == durable.remote_revision
                        && base.content_hash == snapshot.sha256.as_str()
                })
            });
            if exact_base_match {
                let remote = matching_remote.ok_or_else(SyncCommandError::internal)?;
                let operation_id = transfer_operation_id(&[
                    "upload",
                    &local.path,
                    &remote.parent_id,
                    "absent",
                    "absent",
                    &snapshot.revision.hex,
                    snapshot.sha256.as_str(),
                    &snapshot.byte_len.to_string(),
                ]);
                if requeue_prior_absent_upload_if_exact(
                    store,
                    operation_id,
                    &local.path,
                    &remote.parent_id,
                    &snapshot.revision.hex,
                    snapshot.sha256.as_str(),
                    snapshot.byte_len,
                    content_kind(&local.path, matches!(local.kind, ExplorerKindDto::Markdown)),
                    reconciliation_time,
                    None,
                )? {
                    registered = registered.saturating_add(1);
                }
                continue;
            }
            let parent_id = if let Some(remote) = matching_remote {
                remote.parent_id.as_str()
            } else if let Some(parent) = parent_path(&local.path) {
                remote_folders.get(parent).copied().ok_or_else(|| {
                    SyncCommandError::new(
                        SyncCommandCode::InvalidRequest,
                        "a required remote parent folder does not exist",
                    )
                })?
            } else {
                root_id
            };
            let remote_identity = matching_remote.map_or("absent", |entry| entry.file_id.as_str());
            let remote_revision = matching_durable
                .as_ref()
                .map_or("absent", |entry| entry.remote_revision.as_str());
            let operation_id = transfer_operation_id(&[
                "upload",
                &local.path,
                parent_id,
                remote_identity,
                remote_revision,
                &snapshot.revision.hex,
                snapshot.sha256.as_str(),
                &snapshot.byte_len.to_string(),
            ]);
            let mime_class =
                content_kind(&local.path, matches!(local.kind, ExplorerKindDto::Markdown));
            let record = TransferRecord::new(
                operation_id,
                TransferDirection::Upload,
                local.path,
                parent_id,
                matching_remote.map(|entry| entry.file_id.clone()),
                Some(snapshot.revision.hex),
                matching_durable.map(|entry| entry.remote_revision),
                snapshot.sha256.as_str(),
                snapshot.byte_len,
                mime_class,
                operation_marker(operation_id),
                Some(stage_reference(operation_id)),
                None,
                0,
            )
            .map_err(map_sync_error)?;
            if register_or_reconcile(store, &record, reconciliation_time)? {
                registered = registered.saturating_add(1);
            }
        }

        for remote in remote_entries
            .iter()
            .filter(|entry| entry.kind == RemoteEntryKind::File)
            .filter(|entry| !local_paths.contains(entry.path.as_str()))
        {
            let durable = store
                .remote_entry(&remote.file_id)
                .map_err(map_sync_error)?
                .ok_or_else(SyncCommandError::internal)?;
            let name = display_name(&remote.path)?;
            let candidate = drive
                .inspect_download_candidate(
                    &remote.file_id,
                    &remote.parent_id,
                    name,
                    &durable.remote_revision,
                )
                .map_err(map_drive_error)?;
            let operation_id = transfer_operation_id(&[
                "download",
                &remote.path,
                &remote.parent_id,
                candidate.file_id(),
                candidate.sync_revision(),
                candidate.sha256(),
                &candidate.size().to_string(),
            ]);
            let record = TransferRecord::new(
                operation_id,
                TransferDirection::Download,
                remote.path.clone(),
                remote.parent_id.clone(),
                Some(remote.file_id.clone()),
                None,
                Some(candidate.sync_revision().to_owned()),
                candidate.sha256(),
                candidate.size(),
                content_kind(&remote.path, false),
                operation_marker(operation_id),
                Some(stage_reference(operation_id)),
                None,
                0,
            )
            .map_err(map_sync_error)?;
            if register_or_reconcile(store, &record, reconciliation_time)? {
                registered = registered.saturating_add(1);
            }
        }
        Ok(registered)
    }

    #[derive(Debug)]
    struct IncrementalBatch {
        batch_id: Uuid,
        final_page: bool,
    }

    fn prepare_incremental_change_batch(
        service: &AppService,
        session_id: VaultSessionId,
        store: &mut SyncStore,
        read_only: &ReadOnlyDrive,
        transfer_drive: &TransferDrive,
        root_id: &str,
    ) -> Result<IncrementalBatch, SyncCommandError> {
        let state = store
            .vault_state()
            .map_err(map_sync_error)?
            .ok_or_else(SyncCommandError::internal)?;
        let account_id = state.account_id.as_deref().ok_or_else(|| {
            SyncCommandError::new(
                SyncCommandCode::BindingMismatch,
                "an exact Drive account must be bound before transfer",
            )
        })?;
        let binding = read_only
            .verify_binding(account_id, root_id)
            .map_err(map_drive_error)?;
        store
            .verify_remote_binding(&binding)
            .map_err(map_sync_error)?;
        let cursor = state.durable_cursor.as_deref().ok_or_else(|| {
            SyncCommandError::new(
                SyncCommandCode::RescanRequired,
                "Drive metadata must have a durable cursor before transfer",
            )
        })?;
        let page = map_drive_with_rescan(store, read_only.changes_page(cursor))?;
        let next_cursor = page
            .next_page_token
            .as_deref()
            .or(page.new_start_page_token.as_deref())
            .ok_or_else(SyncCommandError::internal)?;
        let final_page = page.new_start_page_token.is_some();
        let mut changes = Vec::new();
        let mut downloads = Vec::new();
        let mut merged_remote_paths = collect_remote_preview(store)?
            .into_iter()
            .map(|entry| (entry.file_id, entry.path))
            .collect::<BTreeMap<_, _>>();

        for raw in &page.changes {
            let known = store.remote_entry(&raw.file_id).map_err(map_sync_error)?;
            match map_drive_with_rescan(store, read_only.resolve_change_below_root(root_id, raw))? {
                ResolvedDriveChange::Removed { .. } | ResolvedDriveChange::OutsideBoundRoot => {
                    if known.is_some() {
                        store
                            .mark_rescan_required(now_unix_ms()?)
                            .map_err(map_sync_error)?;
                        return Err(SyncCommandError::new(
                            SyncCommandCode::RescanRequired,
                            "remote move or removal requires explicit reconciliation",
                        ));
                    }
                }
                ResolvedDriveChange::Inside(entry) => {
                    if !myvault_sync_engine::is_valid_sync_content_path(&entry.path) {
                        store
                            .mark_rescan_required(now_unix_ms()?)
                            .map_err(map_sync_error)?;
                        return Err(SyncCommandError::new(
                            SyncCommandCode::RescanRequired,
                            "protected or invalid remote paths require explicit reconciliation",
                        ));
                    }
                    if let Some(previous) = known.as_ref() {
                        if previous.path != entry.path
                            || previous.parent_id != entry.parent_id
                            || previous.kind != entry.kind
                        {
                            store
                                .mark_rescan_required(now_unix_ms()?)
                                .map_err(map_sync_error)?;
                            return Err(SyncCommandError::new(
                                SyncCommandCode::RescanRequired,
                                "remote move or rename requires explicit reconciliation",
                            ));
                        }
                    }
                    merged_remote_paths.insert(entry.file_id.clone(), entry.path.clone());
                    let requires_download = entry.kind == RemoteEntryKind::File
                        && known.as_ref().is_none_or(|previous| {
                            previous.remote_revision != entry.remote_revision
                                || previous.content_hash != entry.content_hash
                        });
                    if requires_download {
                        let name = display_name(&entry.path)?;
                        let candidate = transfer_drive
                            .inspect_download_candidate(
                                &entry.file_id,
                                &entry.parent_id,
                                name,
                                &entry.remote_revision,
                            )
                            .map_err(map_drive_error)?;
                        let operation_id = transfer_operation_id(&[
                            "download",
                            &entry.path,
                            &entry.parent_id,
                            candidate.file_id(),
                            candidate.sync_revision(),
                            candidate.sha256(),
                            &candidate.size().to_string(),
                        ]);
                        downloads.push(
                            TransferRecord::new(
                                operation_id,
                                TransferDirection::Download,
                                entry.path.clone(),
                                entry.parent_id.clone(),
                                Some(entry.file_id.clone()),
                                None,
                                Some(candidate.sync_revision().to_owned()),
                                candidate.sha256(),
                                candidate.size(),
                                content_kind(&entry.path, false),
                                operation_marker(operation_id),
                                Some(stage_reference(operation_id)),
                                None,
                                0,
                            )
                            .map_err(map_sync_error)?,
                        );
                    }
                    changes.push(RemoteChange::Upsert(entry));
                }
            }
        }

        let local_entries = collect_local_entries(service, session_id)?;
        let mut exact_remote_paths = BTreeSet::new();
        if merged_remote_paths
            .values()
            .any(|path| !exact_remote_paths.insert(path.as_str()))
        {
            store
                .mark_rescan_required(now_unix_ms()?)
                .map_err(map_sync_error)?;
            return Err(SyncCommandError::new(
                SyncCommandCode::RescanRequired,
                "duplicate remote paths require explicit reconciliation",
            ));
        }
        reject_portable_path_collisions(
            merged_remote_paths
                .values()
                .map(String::as_str)
                .chain(local_entries.iter().map(|entry| entry.path.as_str())),
        )?;
        let batch_id = transfer_operation_id(&["changes", cursor, next_cursor]);
        store
            .begin_transfer_change_batch(batch_id, cursor, next_cursor, &changes, &downloads)
            .map_err(map_sync_error)?;
        Ok(IncrementalBatch {
            batch_id,
            final_page,
        })
    }

    fn requeue_active_batch_reconciliation(
        store: &mut SyncStore,
        batch_id: Uuid,
        now: u64,
    ) -> Result<(), SyncCommandError> {
        for mutation in store.local_mutations(batch_id).map_err(map_sync_error)? {
            let operation_id =
                Uuid::parse_str(&mutation.mutation_id).map_err(|_| SyncCommandError::internal())?;
            let transfer = store
                .transfer(operation_id)
                .map_err(map_sync_error)?
                .ok_or_else(SyncCommandError::internal)?;
            if transfer.phase == TransferPhase::NeedsReconcile {
                store
                    .requeue_transfer_for_reconciliation(
                        operation_id,
                        now.max(transfer.updated_at_unix_ms),
                    )
                    .map_err(map_sync_error)?;
            }
        }
        Ok(())
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum GuardedWorkerOutcome {
        Drained,
        AuthRequired,
    }

    fn run_guarded_worker(
        service: &AppService,
        session_id: VaultSessionId,
        store: &mut SyncStore,
        drive: TransferDrive,
    ) -> Result<GuardedWorkerOutcome, SyncCommandError> {
        let executor = NativeTransferExecutor::new(service, session_id, drive);
        let mut worker = Worker::new(store, executor);
        for _ in 0..MAX_GUARDED_OPERATIONS {
            match worker
                .run_once(now_unix_ms()?)
                .map_err(|_| SyncCommandError::internal())?
            {
                WorkOutcome::Idle => return Ok(GuardedWorkerOutcome::Drained),
                WorkOutcome::Completed(_)
                | WorkOutcome::RetryScheduled(_)
                | WorkOutcome::NeedsReconcile(_) => {}
                WorkOutcome::AuthRequired(_) => return Ok(GuardedWorkerOutcome::AuthRequired),
            }
        }
        Ok(GuardedWorkerOutcome::Drained)
    }

    fn run_guarded_worker_with_auth_refresh(
        service: &AppService,
        session_id: VaultSessionId,
        store: &mut SyncStore,
        account_id: &str,
        root_id: &str,
        access: &mut FreshAccessToken,
        refresh_attempted: &mut bool,
    ) -> Result<GuardedWorkerOutcome, SyncCommandError> {
        let first = run_guarded_worker(
            service,
            session_id,
            store,
            transfer_drive(access, account_id, root_id)?,
        )?;
        if first != GuardedWorkerOutcome::AuthRequired
            || !refresh_run_access_once(account_id, access, refresh_attempted)?
        {
            return Ok(first);
        }
        store
            .resume_auth_required_transfers(now_unix_ms()?)
            .map_err(map_sync_error)?;
        run_guarded_worker(
            service,
            session_id,
            store,
            transfer_drive(access, account_id, root_id)?,
        )
    }

    fn require_connected_account(
        connected_account_id: Option<&str>,
        requested_account_id: &str,
    ) -> Result<(), SyncCommandError> {
        if connected_account_id == Some(requested_account_id) {
            Ok(())
        } else {
            Err(SyncCommandError::new(
                SyncCommandCode::BindingMismatch,
                "the Google account does not match the connected account",
            ))
        }
    }

    fn require_compatible_bound_account(
        bound_account_id: Option<&str>,
        observed_account_id: &str,
    ) -> Result<(), SyncCommandError> {
        if bound_account_id.is_none() || bound_account_id == Some(observed_account_id) {
            Ok(())
        } else {
            Err(SyncCommandError::new(
                SyncCommandCode::BindingMismatch,
                "the Google account does not match this Vault's exact binding",
            ))
        }
    }

    fn status_impl(
        service: &AppService,
        runtime: &SyncRuntime,
        session_id: VaultSessionId,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        service
            .with_native_session_lease(session_id, |context| {
                let mut inner = runtime
                    .inner
                    .lock()
                    .map_err(|_| SyncCommandError::internal())?;
                let (state, transfers) = {
                    let store = ensure_store(&mut inner, &context)?;
                    (
                        store.vault_state().map_err(map_sync_error)?,
                        store.transfer_summary().map_err(map_sync_error)?,
                    )
                };
                refresh_connected_state(&mut inner, state.as_ref())?;
                Ok(status_from(session_id, &inner, state.as_ref(), transfers))
            })
            .map_err(map_app_error)?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_status(
        service: tauri::State<'_, Arc<AppService>>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let session_id = parse_session_id(&session_id)?;
        let service = Arc::clone(service.inner());
        let runtime = Arc::clone(runtime.inner());
        tauri::async_runtime::spawn_blocking(move || status_impl(&service, &runtime, session_id))
            .await
            .map_err(|_| SyncCommandError::internal())?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_connect(
        service: tauri::State<'_, Arc<AppService>>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let session_id = parse_session_id(&session_id)?;
        let service = Arc::clone(service.inner());
        let runtime = Arc::clone(runtime.inner());
        tauri::async_runtime::spawn_blocking(move || {
            service
                .with_native_session_lease(session_id, |context| {
                    let client_id = desktop_client_id()?;
                    let provider = provider(&client_id)?;
                    let mut inner = runtime
                        .inner
                        .lock()
                        .map_err(|_| SyncCommandError::internal())?;
                    let bound_account_id = ensure_store(&mut inner, &context)?
                        .vault_state()
                        .map_err(map_sync_error)?
                        .and_then(|state| state.account_id);
                    let flow =
                        DesktopOAuth::bind(&client_id, &[myvault_desktop_auth::GOOGLE_DRIVE_SCOPE])
                            .map_err(|_| {
                                SyncCommandError::new(
                                    SyncCommandCode::ProviderUnavailable,
                                    "Google authorization could not be started",
                                )
                            })?;
                    launch_system_browser(flow.authorization_url().as_str())?;
                    let request = flow.wait_for_callback(CALLBACK_TIMEOUT).map_err(|_| {
                        SyncCommandError::new(
                            SyncCommandCode::AuthRequired,
                            "Google authorization was not completed",
                        )
                    })?;
                    let tokens = provider.exchange(&request).map_err(|_| {
                        SyncCommandError::new(
                            SyncCommandCode::AuthRequired,
                            "Google authorization could not be completed",
                        )
                    })?;
                    let drive =
                        ReadOnlyDrive::google(AccessToken::new(tokens.access_token().to_owned()))
                            .map_err(map_drive_error)?;
                    let account = drive.account_identity().map_err(map_drive_error)?;
                    require_compatible_bound_account(
                        bound_account_id.as_deref(),
                        &account.permission_id,
                    )?;
                    let refresh = tokens.refresh_token.as_ref().ok_or_else(|| {
                        SyncCommandError::new(
                            SyncCommandCode::AuthRequired,
                            "Google did not return an offline refresh credential",
                        )
                    })?;
                    provider
                        .save_refresh_token(&account.permission_id, refresh)
                        .map_err(|_| {
                            SyncCommandError::new(
                                SyncCommandCode::StorageUnavailable,
                                "secure credential storage is unavailable",
                            )
                        })?;
                    inner.connected_account_id = Some(account.permission_id);
                    let (state, transfers) = {
                        let store = ensure_store(&mut inner, &context)?;
                        (
                            store.vault_state().map_err(map_sync_error)?,
                            store.transfer_summary().map_err(map_sync_error)?,
                        )
                    };
                    Ok(status_from(session_id, &inner, state.as_ref(), transfers))
                })
                .map_err(map_app_error)?
        })
        .await
        .map_err(|_| SyncCommandError::internal())?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_list_folders(
        service: tauri::State<'_, Arc<AppService>>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
        parent_id: Option<String>,
        page_token: Option<String>,
    ) -> Result<RemoteFolderPageDto, SyncCommandError> {
        let session_id = parse_session_id(&session_id)?;
        let service = Arc::clone(service.inner());
        let runtime = Arc::clone(runtime.inner());
        tauri::async_runtime::spawn_blocking(move || {
            service
                .with_native_session_lease(session_id, |_| {
                    let inner = runtime
                        .inner
                        .lock()
                        .map_err(|_| SyncCommandError::internal())?;
                    let account_id = inner.connected_account_id.as_deref().ok_or_else(|| {
                        SyncCommandError::new(
                            SyncCommandCode::AuthRequired,
                            "Google authorization is required",
                        )
                    })?;
                    let drive = fresh_drive(account_id)?;
                    let observed = drive.account_identity().map_err(map_drive_error)?;
                    if observed.permission_id != account_id {
                        return Err(SyncCommandError::new(
                            SyncCommandCode::BindingMismatch,
                            "the Google account does not match the connected account",
                        ));
                    }
                    let requested_parent = parent_id.clone();
                    let parent = requested_parent.as_deref().unwrap_or("root");
                    let page = drive
                        .list_children_page(parent, page_token.as_deref())
                        .map_err(map_drive_error)?;
                    Ok(folder_page(session_id, requested_parent, page))
                })
                .map_err(map_app_error)?
        })
        .await
        .map_err(|_| SyncCommandError::internal())?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_bind_root(
        service: tauri::State<'_, Arc<AppService>>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
        account_id: String,
        root_id: String,
    ) -> Result<BindRootDto, SyncCommandError> {
        let session_id = parse_session_id(&session_id)?;
        let service = Arc::clone(service.inner());
        let runtime = Arc::clone(runtime.inner());
        tauri::async_runtime::spawn_blocking(move || {
            service
                .with_native_session_lease(session_id, |context| {
                    let mut inner = runtime
                        .inner
                        .lock()
                        .map_err(|_| SyncCommandError::internal())?;
                    require_connected_account(inner.connected_account_id.as_deref(), &account_id)?;
                    let drive = fresh_drive(&account_id)?;
                    let root = drive.verify_root(&root_id).map_err(map_drive_error)?;
                    let binding = drive
                        .verify_binding(&account_id, &root_id)
                        .map_err(map_drive_error)?;
                    let (outcome, state, transfers) = {
                        let store = ensure_store(&mut inner, &context)?;
                        let outcome = store
                            .bind_remote_root(&binding, now_unix_ms()?)
                            .map_err(map_sync_error)?;
                        let state = store.vault_state().map_err(map_sync_error)?;
                        let transfers = store.transfer_summary().map_err(map_sync_error)?;
                        (outcome, state, transfers)
                    };
                    inner.root_name = Some(root.name);
                    Ok(BindRootDto {
                        session_id,
                        outcome: match outcome {
                            BindOutcome::Created => "created",
                            BindOutcome::AlreadyBound => "alreadyBound",
                            BindOutcome::LegacyBindingConfirmed => "legacyBindingConfirmed",
                        },
                        status: status_from(session_id, &inner, state.as_ref(), transfers),
                    })
                })
                .map_err(map_app_error)?
        })
        .await
        .map_err(|_| SyncCommandError::internal())?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_scan_step(
        service: tauri::State<'_, Arc<AppService>>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<ScanStepDto, SyncCommandError> {
        let session_id = parse_session_id(&session_id)?;
        let service = Arc::clone(service.inner());
        let runtime = Arc::clone(runtime.inner());
        tauri::async_runtime::spawn_blocking(move || {
            service
                .with_native_session_lease(session_id, |context| {
                    let mut inner = runtime
                        .inner
                        .lock()
                        .map_err(|_| SyncCommandError::internal())?;
                    let account_id = inner.connected_account_id.clone().ok_or_else(|| {
                        SyncCommandError::new(
                            SyncCommandCode::AuthRequired,
                            "Google authorization is required",
                        )
                    })?;
                    let mut drive = fresh_drive(&account_id)?;
                    let store = ensure_store(&mut inner, &context)?;
                    let state = store
                        .vault_state()
                        .map_err(map_sync_error)?
                        .ok_or_else(|| {
                            SyncCommandError::new(
                                SyncCommandCode::BindingMismatch,
                                "an exact Drive root must be bound before scanning",
                            )
                        })?;
                    let binding = drive
                        .verify_binding(&account_id, &state.remote_root_id)
                        .map_err(map_drive_error)?;
                    store
                        .verify_remote_binding(&binding)
                        .map_err(map_sync_error)?;
                    let progress = advance_initial_sync(store, &mut drive, now_unix_ms()?)
                        .map_err(map_sync_error)?;
                    let state = store.vault_state().map_err(map_sync_error)?;
                    let transfers = store.transfer_summary().map_err(map_sync_error)?;
                    Ok(ScanStepDto {
                        session_id,
                        progress: progress_name(progress),
                        status: status_from(session_id, &inner, state.as_ref(), transfers),
                    })
                })
                .map_err(map_app_error)?
        })
        .await
        .map_err(|_| SyncCommandError::internal())?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_preview(
        service: tauri::State<'_, Arc<AppService>>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
        after: Option<String>,
        limit: Option<usize>,
    ) -> Result<RemotePreviewPageDto, SyncCommandError> {
        let session_id = parse_session_id(&session_id)?;
        let cursor = decode_preview_cursor(after.as_deref())?;
        let limit = limit.unwrap_or(DEFAULT_PREVIEW_LIMIT);
        let service = Arc::clone(service.inner());
        let runtime = Arc::clone(runtime.inner());
        tauri::async_runtime::spawn_blocking(move || {
            service
                .with_native_session_lease(session_id, |context| {
                    let mut inner = runtime
                        .inner
                        .lock()
                        .map_err(|_| SyncCommandError::internal())?;
                    let page = ensure_store(&mut inner, &context)?
                        .remote_preview(cursor.as_ref(), limit)
                        .map_err(map_sync_error)?;
                    preview_page(session_id, page)
                })
                .map_err(map_app_error)?
        })
        .await
        .map_err(|_| SyncCommandError::internal())?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_run_guarded(
        service: tauri::State<'_, Arc<AppService>>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let session_id = parse_session_id(&session_id)?;
        let service = Arc::clone(service.inner());
        let runtime = Arc::clone(runtime.inner());
        tauri::async_runtime::spawn_blocking(move || {
            let context = service
                .native_vault_context(session_id)
                .map_err(map_app_error)?;
            let (account_id, bound, mut detached) = {
                let mut inner = runtime
                    .inner
                    .lock()
                    .map_err(|_| SyncCommandError::internal())?;
                let account_id = inner.connected_account_id.clone().ok_or_else(|| {
                    SyncCommandError::new(
                        SyncCommandCode::AuthRequired,
                        "Google authorization is required",
                    )
                })?;
                let bound = ensure_store(&mut inner, &context)?
                    .vault_state()
                    .map_err(map_sync_error)?
                    .ok_or_else(|| {
                        SyncCommandError::new(
                            SyncCommandCode::BindingMismatch,
                            "an exact Drive root must be bound before transfer",
                        )
                    })?;
                if bound.phase != SyncPhase::Ready || bound.rescan_required {
                    return Err(SyncCommandError::new(
                        SyncCommandCode::RescanRequired,
                        "Drive metadata must be fully scanned before transfer",
                    ));
                }
                require_connected_account(bound.account_id.as_deref(), &account_id)?;
                let active = inner.active.take().ok_or_else(SyncCommandError::internal)?;
                inner.transfer_running_session = Some(session_id);
                (
                    account_id,
                    bound,
                    DetachedActiveSync {
                        runtime: Arc::clone(&runtime),
                        active: Some(active),
                    },
                )
            };

            let run_result = (|| {
                let store = detached.store_mut()?;
                store
                    .resume_auth_required_transfers(now_unix_ms()?)
                    .map_err(map_sync_error)?;

                let mut access = fresh_access(&account_id)?;
                let mut refresh_attempted = false;
                let mut metadata_fresh = false;
                let mut auth_blocked = false;
                for page_index in 0..MAX_INCREMENTAL_PAGES {
                    let active = store.active_change_batch().map_err(map_sync_error)?;
                    let (batch_id, final_page) = if let Some(active) = active {
                        requeue_active_batch_reconciliation(
                            store,
                            active.batch_id,
                            now_unix_ms()?,
                        )?;
                        (active.batch_id, false)
                    } else {
                        let batch = loop {
                            let read_only = read_only_drive(&access)?;
                            let drive =
                                transfer_drive(&access, &account_id, &bound.remote_root_id)?;
                            match prepare_incremental_change_batch(
                                &service,
                                session_id,
                                store,
                                &read_only,
                                &drive,
                                &bound.remote_root_id,
                            ) {
                                Err(error)
                                    if error.code == SyncCommandCode::AuthRequired
                                        && refresh_run_access_once(
                                            &account_id,
                                            &mut access,
                                            &mut refresh_attempted,
                                        )? => {}
                                result => break result?,
                            }
                        };
                        (batch.batch_id, batch.final_page)
                    };

                    if run_guarded_worker_with_auth_refresh(
                        &service,
                        session_id,
                        store,
                        &account_id,
                        &bound.remote_root_id,
                        &mut access,
                        &mut refresh_attempted,
                    )? == GuardedWorkerOutcome::AuthRequired
                    {
                        auth_blocked = true;
                        break;
                    }
                    let active = store
                        .active_change_batch()
                        .map_err(map_sync_error)?
                        .ok_or_else(SyncCommandError::internal)?;
                    if active.applying_mutations == 0
                        && active.committed_mutations == active.declared_mutations
                    {
                        store
                            .commit_transfer_change_batch(batch_id, now_unix_ms()?)
                            .map_err(map_sync_error)?;
                        if final_page {
                            metadata_fresh = true;
                            break;
                        }
                    } else {
                        break;
                    }
                    if page_index + 1 == MAX_INCREMENTAL_PAGES {
                        return Err(SyncCommandError::new(
                            SyncCommandCode::RescanRequired,
                            "the bounded Drive changes drain did not finish",
                        ));
                    }
                }

                if metadata_fresh && !auth_blocked {
                    loop {
                        let drive = transfer_drive(&access, &account_id, &bound.remote_root_id)?;
                        match prepare_guarded_transfers(
                            &service,
                            session_id,
                            store,
                            &drive,
                            &bound.remote_root_id,
                        ) {
                            Err(error)
                                if error.code == SyncCommandCode::AuthRequired
                                    && refresh_run_access_once(
                                        &account_id,
                                        &mut access,
                                        &mut refresh_attempted,
                                    )? => {}
                            result => {
                                result?;
                                break;
                            }
                        }
                    }
                    let _ = run_guarded_worker_with_auth_refresh(
                        &service,
                        session_id,
                        store,
                        &account_id,
                        &bound.remote_root_id,
                        &mut access,
                        &mut refresh_attempted,
                    )?;
                }
                service
                    .confirm_active_session(session_id)
                    .map_err(map_app_error)
            })();
            if let Err(error) = run_result {
                detached.discard_store()?;
                return Err(error);
            }
            detached.restore()?;
            status_impl(&service, &runtime, session_id)
        })
        .await
        .map_err(|_| SyncCommandError::internal())?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_disconnect(
        service: tauri::State<'_, Arc<AppService>>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let session_id = parse_session_id(&session_id)?;
        let service = Arc::clone(service.inner());
        let runtime = Arc::clone(runtime.inner());
        tauri::async_runtime::spawn_blocking(move || {
            service
                .with_native_session_lease(session_id, |context| {
                    let mut inner = runtime
                        .inner
                        .lock()
                        .map_err(|_| SyncCommandError::internal())?;
                    let persisted_account_id = ensure_store(&mut inner, &context)?
                        .vault_state()
                        .map_err(map_sync_error)?
                        .and_then(|state| state.account_id);
                    let account_id =
                        persisted_account_id.or_else(|| inner.connected_account_id.clone());
                    if let Some(account_id) = account_id {
                        OsKeyringStore::new(KEYRING_SERVICE)
                            .delete_refresh_token(&account_id)
                            .map_err(|_| {
                                SyncCommandError::new(
                                    SyncCommandCode::StorageUnavailable,
                                    "secure credential cleanup did not complete",
                                )
                            })?;
                    }
                    inner.connected_account_id = None;
                    inner.root_name = None;
                    let (state, transfers) = {
                        let store = ensure_store(&mut inner, &context)?;
                        (
                            store.vault_state().map_err(map_sync_error)?,
                            store.transfer_summary().map_err(map_sync_error)?,
                        )
                    };
                    Ok(status_from(session_id, &inner, state.as_ref(), transfers))
                })
                .map_err(map_app_error)?
        })
        .await
        .map_err(|_| SyncCommandError::internal())?
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use myvault_core::Vault;

        struct EmptyInitialDrive;

        impl myvault_sync_engine::DriveClient for EmptyInitialDrive {
            fn get_start_page_token(
                &mut self,
            ) -> std::result::Result<String, myvault_sync_engine::RemoteError> {
                Ok("cursor_1".to_owned())
            }

            fn scan_folder_page(
                &mut self,
                _request: &myvault_sync_engine::ScanRequest,
            ) -> std::result::Result<myvault_sync_engine::ScanPage, myvault_sync_engine::RemoteError>
            {
                Ok(myvault_sync_engine::ScanPage {
                    entries: Vec::new(),
                    next_page_token: None,
                })
            }

            fn changes_page(
                &mut self,
                _page_token: &str,
            ) -> std::result::Result<
                myvault_sync_engine::ChangesPage,
                myvault_sync_engine::RemoteError,
            > {
                Ok(myvault_sync_engine::ChangesPage {
                    changes: Vec::new(),
                    next_page_token: None,
                    new_start_page_token: Some("cursor_1".to_owned()),
                })
            }
        }

        struct SingleFileInitialDrive {
            entry: Option<myvault_sync_engine::RemoteEntry>,
        }

        impl myvault_sync_engine::DriveClient for SingleFileInitialDrive {
            fn get_start_page_token(
                &mut self,
            ) -> std::result::Result<String, myvault_sync_engine::RemoteError> {
                Ok("cursor_1".to_owned())
            }

            fn scan_folder_page(
                &mut self,
                _request: &myvault_sync_engine::ScanRequest,
            ) -> std::result::Result<myvault_sync_engine::ScanPage, myvault_sync_engine::RemoteError>
            {
                Ok(myvault_sync_engine::ScanPage {
                    entries: self.entry.take().into_iter().collect(),
                    next_page_token: None,
                })
            }

            fn changes_page(
                &mut self,
                _page_token: &str,
            ) -> std::result::Result<
                myvault_sync_engine::ChangesPage,
                myvault_sync_engine::RemoteError,
            > {
                Ok(myvault_sync_engine::ChangesPage {
                    changes: Vec::new(),
                    next_page_token: None,
                    new_start_page_token: Some("cursor_1".to_owned()),
                })
            }
        }

        #[test]
        fn desktop_client_id_and_exact_connected_account_are_fail_closed() {
            assert!(validate_client_id("client-123.apps.googleusercontent.com").is_ok());
            for invalid in ["", "client id", "client\nsecret", "บัญชี"] {
                assert_eq!(
                    validate_client_id(invalid)
                        .expect_err("invalid client")
                        .code,
                    SyncCommandCode::Unconfigured
                );
            }
            assert!(require_connected_account(Some("account_1"), "account_1").is_ok());
            assert_eq!(
                require_connected_account(Some("account_1"), "account_2")
                    .expect_err("wrong account")
                    .code,
                SyncCommandCode::BindingMismatch
            );
            assert_eq!(
                require_connected_account(None, "account_1")
                    .expect_err("disconnected")
                    .code,
                SyncCommandCode::BindingMismatch
            );
            assert!(require_compatible_bound_account(None, "account_1").is_ok());
            assert!(require_compatible_bound_account(Some("account_1"), "account_1").is_ok());
            assert_eq!(
                require_compatible_bound_account(Some("account_1"), "account_2")
                    .expect_err("wrong bound account")
                    .code,
                SyncCommandCode::BindingMismatch
            );
        }

        #[test]
        fn guarded_run_claims_exactly_one_serialized_auth_refresh() {
            let mut attempted = false;
            assert!(claim_auth_refresh(&mut attempted));
            assert!(attempted);
            assert!(!claim_auth_refresh(&mut attempted));
        }

        #[test]
        fn transfer_identity_is_deterministic_and_evidence_bound() {
            let first = transfer_operation_id(&[
                "upload",
                "Notes/one.md",
                "root_1",
                "absent",
                "revision_1",
            ]);
            assert_eq!(
                first,
                transfer_operation_id(&[
                    "upload",
                    "Notes/one.md",
                    "root_1",
                    "absent",
                    "revision_1",
                ])
            );
            assert_ne!(
                first,
                transfer_operation_id(&[
                    "upload",
                    "Notes/one.md",
                    "root_1",
                    "absent",
                    "revision_2",
                ])
            );
            assert_eq!(stage_reference(first), format!("stage-{first}"));
            assert!(!operation_marker(first).contains('/'));
        }

        #[test]
        fn transfer_preflight_rejects_case_and_unicode_collisions() {
            assert!(reject_portable_path_collisions(["Notes/one.md", "Notes/two.md"]).is_ok());
            assert_eq!(
                reject_portable_path_collisions(["Notes/One.md", "notes/one.md"])
                    .expect_err("case collision")
                    .code,
                SyncCommandCode::InvalidRequest
            );
            assert_eq!(
                reject_portable_path_collisions(["Cafe\u{301}.md", "Caf\u{e9}.md"])
                    .expect_err("Unicode collision")
                    .code,
                SyncCommandCode::InvalidRequest
            );
        }

        #[test]
        fn transfer_mime_class_accepts_markdown_extensions_case_insensitively() {
            assert_eq!(
                content_kind("Notes/one.MD", false),
                TransferMimeClass::Markdown
            );
            assert_eq!(
                content_kind("attachments/archive.bin", false),
                TransferMimeClass::Blob
            );
        }

        #[test]
        fn runtime_rejects_a_stale_session_before_opening_sync_state() {
            let temporary = tempfile::tempdir().expect("temporary roots");
            let vault_root = temporary.path().join("vault");
            std::fs::create_dir(&vault_root).expect("vault root");
            let service = AppService::new();
            let vault_root = vault_root.canonicalize().expect("canonical vault root");
            service
                .activate_trusted_vault(Vault::open(&vault_root).expect("open vault"))
                .expect("activate vault");
            let runtime = SyncRuntime::default();

            let error = status_impl(&service, &runtime, VaultSessionId::new())
                .expect_err("stale session rejected");

            assert_eq!(error.code, SyncCommandCode::StaleSession);
        }

        #[test]
        fn switching_vaults_clears_session_scoped_root_display_metadata() {
            let temporary = tempfile::tempdir().expect("temporary roots");
            let base = temporary
                .path()
                .canonicalize()
                .expect("canonical temporary root");
            let app_data = base.join("app-data");
            let first_root = base.join("vault-a");
            let second_root = base.join("vault-b");
            std::fs::create_dir(&app_data).expect("app data root");
            std::fs::create_dir(&first_root).expect("first vault root");
            std::fs::create_dir(&second_root).expect("second vault root");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&app_data, std::fs::Permissions::from_mode(0o700))
                    .expect("private app data permissions");
            }
            let service = AppService::with_app_data_root(&app_data);
            let first = service
                .activate_trusted_vault(
                    Vault::open(first_root.canonicalize().expect("canonical first vault"))
                        .expect("open first vault"),
                )
                .expect("activate first vault")
                .session_id
                .expect("first session");
            let runtime = SyncRuntime::default();
            status_impl(&service, &runtime, first).expect("open first sync state");
            {
                let mut inner = runtime.inner.lock().expect("runtime lock");
                inner.root_name = Some("First root".to_owned());
                inner.connected_account_id = Some("account-a".to_owned());
            }

            let second = service
                .activate_trusted_vault(
                    Vault::open(second_root.canonicalize().expect("canonical second vault"))
                        .expect("open second vault"),
                )
                .expect("activate second vault")
                .session_id
                .expect("second session");
            let status = status_impl(&service, &runtime, second).expect("open second sync state");

            assert!(status.root_name.is_none());
            assert!(!status.connected);
            assert!(status.account_id.is_none());
        }

        #[test]
        fn detached_transfer_releases_runtime_lock_and_restores_store_on_drop() {
            let temporary = tempfile::tempdir().expect("temporary roots");
            let base = temporary.path().canonicalize().expect("canonical root");
            let app_data = base.join("app-data");
            let vault_root = base.join("vault");
            std::fs::create_dir(&app_data).expect("app data root");
            std::fs::create_dir(&vault_root).expect("vault root");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&app_data, std::fs::Permissions::from_mode(0o700))
                    .expect("private app data permissions");
            }
            let service = AppService::with_app_data_root(&app_data);
            let session_id = service
                .activate_trusted_vault(
                    Vault::open(vault_root.canonicalize().expect("canonical vault"))
                        .expect("open vault"),
                )
                .expect("activate vault")
                .session_id
                .expect("session");
            let runtime = Arc::new(SyncRuntime::default());
            status_impl(&service, &runtime, session_id).expect("open sync state");

            let detached = {
                let mut inner = runtime.inner.lock().expect("runtime lock");
                let active = inner.active.take().expect("active store");
                inner.transfer_running_session = Some(session_id);
                DetachedActiveSync {
                    runtime: Arc::clone(&runtime),
                    active: Some(active),
                }
            };

            let busy = status_impl(&service, &runtime, session_id).expect_err("busy status");
            assert_eq!(busy.code, SyncCommandCode::Busy);
            drop(detached);
            status_impl(&service, &runtime, session_id).expect("restored sync state");
        }

        #[test]
        fn exact_base_match_requeues_only_the_prior_absent_upload() {
            let temporary = tempfile::tempdir().expect("temporary roots");
            let base = temporary.path().canonicalize().expect("canonical root");
            let app_data = base.join("app-data");
            let vault_root = base.join("vault");
            std::fs::create_dir(&app_data).expect("app data root");
            std::fs::create_dir(&vault_root).expect("vault root");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&app_data, std::fs::Permissions::from_mode(0o700))
                    .expect("private app data permissions");
            }
            let service = AppService::with_app_data_root(&app_data);
            let session_id = service
                .activate_trusted_vault(
                    Vault::open(vault_root.canonicalize().expect("canonical vault"))
                        .expect("open vault"),
                )
                .expect("activate vault")
                .session_id
                .expect("session");
            let context = service.native_vault_context(session_id).expect("context");
            let mut store = SyncStore::open(
                context.app_data_root().expect("app data"),
                context.vault_root(),
                context.vault_id(),
            )
            .expect("sync store");
            let binding = myvault_sync_engine::VerifiedRemoteBinding::new(
                "account_1",
                "root_1",
                "account_1",
                "root_1",
            )
            .expect("binding");
            store.bind_remote_root(&binding, 1).expect("bind");

            let local_revision = "1".repeat(64);
            let sha256 = "2".repeat(64);
            let register_stopped =
                |store: &mut SyncStore, portable_path: &str, error_code: &str| {
                    let operation_id = transfer_operation_id(&[
                        "upload",
                        portable_path,
                        "root_1",
                        "absent",
                        "absent",
                        &local_revision,
                        &sha256,
                        "3",
                    ]);
                    let record = TransferRecord::new(
                        operation_id,
                        TransferDirection::Upload,
                        portable_path,
                        "root_1",
                        None,
                        Some(local_revision.clone()),
                        None,
                        &sha256,
                        3,
                        TransferMimeClass::Markdown,
                        operation_marker(operation_id),
                        Some(stage_reference(operation_id)),
                        None,
                        5,
                    )
                    .expect("transfer evidence");
                    store.register_transfer(&record).expect("register transfer");
                    store
                        .claim_next_transfer(5)
                        .expect("claim transfer")
                        .expect("claimed transfer");
                    store
                        .mark_transfer_needs_reconcile(operation_id, error_code, 6)
                        .expect("stop transfer");
                    operation_id
                };
            let exact = register_stopped(&mut store, "note.md", "drive_revision_mismatch");
            let unrelated = register_stopped(&mut store, "other.md", "drive_revision_mismatch");
            let android_publication_unknown = register_stopped(
                &mut store,
                "android.md",
                "android_local_publication_unknown",
            );

            assert!(requeue_prior_absent_upload_if_exact(
                &mut store,
                exact,
                "note.md",
                "root_1",
                &local_revision,
                &sha256,
                3,
                TransferMimeClass::Markdown,
                7,
                None,
            )
            .expect("requeue exact prior upload"));

            assert!(!requeue_prior_absent_upload_if_exact(
                &mut store,
                android_publication_unknown,
                "android.md",
                "root_1",
                &local_revision,
                &sha256,
                3,
                TransferMimeClass::Markdown,
                7,
                Some("android_local_publication_unknown"),
            )
            .expect("Android publication-unknown evidence remains stopped"));

            let exact = store.transfer(exact).unwrap().unwrap();
            assert_eq!(exact.phase, TransferPhase::RetryScheduled);
            assert_eq!(
                exact.last_error_code.as_deref(),
                Some("reconcile_requested")
            );
            assert_eq!(exact.attempt_count, 1);
            let unrelated = store.transfer(unrelated).unwrap().unwrap();
            assert_eq!(unrelated.phase, TransferPhase::NeedsReconcile);
            assert_eq!(unrelated.attempt_count, 0);
            assert_eq!(
                store
                    .transfer(android_publication_unknown)
                    .unwrap()
                    .unwrap()
                    .phase,
                TransferPhase::NeedsReconcile
            );

            let completed_path = "completed.md";
            let completed = transfer_operation_id(&[
                "upload",
                completed_path,
                "root_1",
                "absent",
                "absent",
                &local_revision,
                &sha256,
                "3",
            ]);
            let completed_record = TransferRecord::new(
                completed,
                TransferDirection::Upload,
                completed_path,
                "root_1",
                None,
                Some(local_revision.clone()),
                None,
                &sha256,
                3,
                TransferMimeClass::Markdown,
                operation_marker(completed),
                Some(stage_reference(completed)),
                None,
                8,
            )
            .expect("completed transfer evidence");
            store
                .register_transfer(&completed_record)
                .expect("register completed transfer");
            let mut claimed = store
                .claim_next_transfer(8)
                .expect("claim transfer")
                .expect("transfer claimed");
            if claimed.operation_id != completed {
                claimed = store
                    .claim_next_transfer(8)
                    .expect("claim completed transfer")
                    .expect("completed transfer claimed");
            }
            assert_eq!(claimed.operation_id, completed);
            let completion = myvault_sync_engine::TransferCompletion::new(
                "remote-file",
                "3".repeat(64),
                local_revision.clone(),
                "base.abcdef",
                "upload_verified",
                9,
            )
            .expect("completion evidence");
            store
                .complete_verified_transfer(completed, &completion)
                .expect("complete transfer");

            assert!(!requeue_prior_absent_upload_if_exact(
                &mut store,
                completed,
                completed_path,
                "root_1",
                &local_revision,
                &sha256,
                3,
                TransferMimeClass::Markdown,
                10,
                None,
            )
            .expect("completed transfers are not reconciliation candidates"));
            assert_eq!(
                store.transfer(completed).unwrap().unwrap().phase,
                TransferPhase::Completed
            );
        }

        #[test]
        fn recovered_absent_upload_completes_with_get_only_remote_verification() {
            const SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
            const REMOTE_REVISION: &str =
                "0000000000000000000000000000000000000000000000000000000000000002";
            let temporary = tempfile::tempdir().expect("temporary roots");
            let base = temporary.path().canonicalize().expect("canonical root");
            let app_data = base.join("app-data");
            let vault_root = base.join("vault");
            std::fs::create_dir(&app_data).expect("app data root");
            std::fs::create_dir(&vault_root).expect("vault root");
            std::fs::write(vault_root.join("note.md"), b"abc").expect("local source");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&app_data, std::fs::Permissions::from_mode(0o700))
                    .expect("private app data permissions");
            }
            let service = AppService::with_app_data_root(&app_data);
            let session_id = service
                .activate_trusted_vault(
                    Vault::open(vault_root.canonicalize().expect("canonical vault"))
                        .expect("open vault"),
                )
                .expect("activate vault")
                .session_id
                .expect("session");
            let mut sink = std::io::sink();
            let snapshot = service
                .stream_transfer_source(
                    session_id,
                    "note.md",
                    &mut sink,
                    MAX_TRANSFER_BYTES as usize,
                )
                .expect("local evidence");
            assert_eq!(snapshot.sha256.as_str(), SHA256);
            let context = service.native_vault_context(session_id).expect("context");
            let mut store = SyncStore::open(
                context.app_data_root().expect("app data"),
                context.vault_root(),
                context.vault_id(),
            )
            .expect("sync store");
            let binding = myvault_sync_engine::VerifiedRemoteBinding::new(
                "account_1",
                "root_1",
                "account_1",
                "root_1",
            )
            .expect("binding");
            store.bind_remote_root(&binding, 1).expect("bind");
            let remote = myvault_sync_engine::RemoteEntry {
                file_id: "file_1".to_owned(),
                parent_id: "root_1".to_owned(),
                path: "note.md".to_owned(),
                kind: RemoteEntryKind::File,
                content_hash: Some(
                    myvault_sync_engine::RemoteContentHash::new(
                        myvault_sync_engine::RemoteHashAlgorithm::Sha256,
                        SHA256,
                    )
                    .expect("remote hash"),
                ),
                remote_revision: REMOTE_REVISION.to_owned(),
            };
            let mut initial = SingleFileInitialDrive {
                entry: Some(remote.clone()),
            };
            for now in 2..=4 {
                advance_initial_sync(&mut store, &mut initial, now).expect("initial step");
            }

            let seed_id = Uuid::new_v4();
            let seed = TransferRecord::new(
                seed_id,
                TransferDirection::Download,
                "note.md",
                "root_1",
                Some("file_1".to_owned()),
                None,
                Some(REMOTE_REVISION.to_owned()),
                SHA256,
                3,
                TransferMimeClass::Markdown,
                operation_marker(seed_id),
                Some(stage_reference(seed_id)),
                None,
                5,
            )
            .expect("seed transfer");
            store.register_transfer(&seed).expect("register seed");
            assert_eq!(
                store
                    .claim_next_transfer(5)
                    .expect("claim seed")
                    .expect("seed claimed")
                    .operation_id,
                seed_id
            );
            let completion = myvault_sync_engine::TransferCompletion::new(
                "file_1",
                REMOTE_REVISION,
                snapshot.revision.hex.clone(),
                "base.seed",
                "download_existing_verified",
                6,
            )
            .expect("seed completion");
            store
                .complete_verified_transfer(seed_id, &completion)
                .expect("publish exact base");

            let operation_id = transfer_operation_id(&[
                "upload",
                "note.md",
                "root_1",
                "absent",
                "absent",
                &snapshot.revision.hex,
                SHA256,
                "3",
            ]);
            let stopped = TransferRecord::new(
                operation_id,
                TransferDirection::Upload,
                "note.md",
                "root_1",
                None,
                Some(snapshot.revision.hex.clone()),
                None,
                SHA256,
                3,
                TransferMimeClass::Markdown,
                operation_marker(operation_id),
                Some(stage_reference(operation_id)),
                None,
                7,
            )
            .expect("stopped transfer");
            store.register_transfer(&stopped).expect("register stopped");
            assert_eq!(
                store
                    .claim_next_transfer(7)
                    .expect("claim stopped")
                    .expect("stopped claimed")
                    .operation_id,
                operation_id
            );
            store
                .mark_transfer_needs_reconcile(operation_id, "drive_revision_mismatch", 8)
                .expect("stop upload");

            let mut server = mockito::Server::new();
            let about = server
                .mock("GET", "/drive/v3/about")
                .match_query(mockito::Matcher::Any)
                .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
                .expect(2)
                .create();
            let root = server
                .mock("GET", "/drive/v3/files/root_1")
                .match_query(mockito::Matcher::Any)
                .with_body(r#"{"id":"root_1","name":"Root","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#)
                .expect(2)
                .create();
            let marker = operation_marker(operation_id);
            let remote_json = format!(
                r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA256}","appProperties":{{"myvaultOperation":"{marker}","myvaultSha256":"{SHA256}","myvaultSize":"3"}}}}"#,
            );
            let query = server
                .mock("GET", "/drive/v3/files")
                .match_query(mockito::Matcher::Any)
                .with_body(format!(r#"{{"files":[{remote_json}]}}"#))
                .expect(1)
                .create();
            let metadata = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(mockito::Matcher::Any)
                .with_body(remote_json)
                .expect(1)
                .create();
            let post = server
                .mock("POST", "/upload/drive/v3/files")
                .match_query(mockito::Matcher::Any)
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

            assert_eq!(
                prepare_guarded_transfers(&service, session_id, &mut store, &drive, "root_1",)
                    .expect("requeue stopped upload"),
                1
            );
            assert_eq!(
                store.transfer(operation_id).unwrap().unwrap().phase,
                TransferPhase::RetryScheduled
            );
            let executor = NativeTransferExecutor::new(&service, session_id, drive);
            let mut worker = Worker::new(&mut store, executor);
            let recovered = worker
                .run_once(now_unix_ms().expect("worker time"))
                .expect("GET-only recovery");
            assert!(matches!(
                recovered,
                WorkOutcome::Completed(completed) if completed == operation_id
            ));
            assert_eq!(
                worker
                    .run_once(now_unix_ms().expect("idle time"))
                    .expect("idle worker"),
                WorkOutcome::Idle
            );
            assert_eq!(
                store.transfer(operation_id).unwrap().unwrap().phase,
                TransferPhase::Completed
            );
            about.assert();
            root.assert();
            query.assert();
            metadata.assert();
            post.assert();
        }

        #[test]
        fn incremental_page_keeps_old_cursor_until_transfer_batch_commit() {
            let temporary = tempfile::tempdir().expect("temporary roots");
            let base = temporary.path().canonicalize().expect("canonical root");
            let app_data = base.join("app-data");
            let vault_root = base.join("vault");
            std::fs::create_dir(&app_data).expect("app data root");
            std::fs::create_dir(&vault_root).expect("vault root");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&app_data, std::fs::Permissions::from_mode(0o700))
                    .expect("private app data permissions");
            }
            let service = AppService::with_app_data_root(&app_data);
            let session_id = service
                .activate_trusted_vault(
                    Vault::open(vault_root.canonicalize().expect("canonical vault"))
                        .expect("open vault"),
                )
                .expect("activate vault")
                .session_id
                .expect("session");
            let context = service.native_vault_context(session_id).expect("context");
            let mut store = SyncStore::open(
                context.app_data_root().expect("app data"),
                context.vault_root(),
                context.vault_id(),
            )
            .expect("sync store");
            let binding = myvault_sync_engine::VerifiedRemoteBinding::new(
                "account_1",
                "root_1",
                "account_1",
                "root_1",
            )
            .expect("binding");
            store.bind_remote_root(&binding, 1).expect("bind");
            let mut initial = EmptyInitialDrive;
            for now in 2..=4 {
                advance_initial_sync(&mut store, &mut initial, now).expect("initial step");
            }
            assert_eq!(
                store
                    .vault_state()
                    .unwrap()
                    .unwrap()
                    .durable_cursor
                    .as_deref(),
                Some("cursor_1")
            );

            let mut server = mockito::Server::new();
            let about = server
                .mock("GET", "/drive/v3/about")
                .match_query(mockito::Matcher::Any)
                .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
                .create();
            let root = server
                .mock("GET", "/drive/v3/files/root_1")
                .match_query(mockito::Matcher::Any)
                .with_body(
                    r#"{"id":"root_1","name":"Fixture","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#,
                )
                .create();
            let changes = server
                .mock("GET", "/drive/v3/changes")
                .match_query(mockito::Matcher::Any)
                .with_body(r#"{"changes":[],"newStartPageToken":"cursor_2"}"#)
                .create();
            let origin = server.url();
            let read_only = ReadOnlyDrive::for_test_origin(&format!("{origin}/drive/v3/"), 4096)
                .expect("read-only test Drive");
            let transfer = TransferDrive::for_test_origins(
                &format!("{origin}/drive/v3/"),
                &format!("{origin}/upload/drive/v3/"),
                "account_1",
                "root_1",
                myvault_transfer::MAX_TRANSFER_BYTES,
            )
            .expect("transfer test Drive");

            let batch = prepare_incremental_change_batch(
                &service, session_id, &mut store, &read_only, &transfer, "root_1",
            )
            .expect("prepare incremental page");
            assert!(batch.final_page);
            assert_eq!(
                store
                    .vault_state()
                    .unwrap()
                    .unwrap()
                    .durable_cursor
                    .as_deref(),
                Some("cursor_1")
            );
            store
                .commit_transfer_change_batch(batch.batch_id, 5)
                .expect("commit zero-mutation page");
            assert_eq!(
                store
                    .vault_state()
                    .unwrap()
                    .unwrap()
                    .durable_cursor
                    .as_deref(),
                Some("cursor_2")
            );
            about.assert();
            root.assert();
            changes.assert();
        }

        #[test]
        fn expired_incremental_cursor_is_durably_invalidated_before_returning() {
            let temporary = tempfile::tempdir().expect("temporary roots");
            let base = temporary.path().canonicalize().expect("canonical root");
            let app_data = base.join("app-data");
            let vault_root = base.join("vault");
            std::fs::create_dir(&app_data).expect("app data root");
            std::fs::create_dir(&vault_root).expect("vault root");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&app_data, std::fs::Permissions::from_mode(0o700))
                    .expect("private app data permissions");
            }
            let service = AppService::with_app_data_root(&app_data);
            let session_id = service
                .activate_trusted_vault(
                    Vault::open(vault_root.canonicalize().expect("canonical vault"))
                        .expect("open vault"),
                )
                .expect("activate vault")
                .session_id
                .expect("session");
            let context = service.native_vault_context(session_id).expect("context");
            let mut store = SyncStore::open(
                context.app_data_root().expect("app data"),
                context.vault_root(),
                context.vault_id(),
            )
            .expect("sync store");
            let binding = myvault_sync_engine::VerifiedRemoteBinding::new(
                "account_1",
                "root_1",
                "account_1",
                "root_1",
            )
            .expect("binding");
            store.bind_remote_root(&binding, 1).expect("bind");
            let mut initial = EmptyInitialDrive;
            for now in 2..=4 {
                advance_initial_sync(&mut store, &mut initial, now).expect("initial step");
            }

            let mut server = mockito::Server::new();
            let about = server
                .mock("GET", "/drive/v3/about")
                .match_query(mockito::Matcher::Any)
                .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
                .create();
            let root = server
                .mock("GET", "/drive/v3/files/root_1")
                .match_query(mockito::Matcher::Any)
                .with_body(
                    r#"{"id":"root_1","name":"Fixture","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#,
                )
                .create();
            let changes = server
                .mock("GET", "/drive/v3/changes")
                .match_query(mockito::Matcher::Any)
                .with_status(410)
                .create();
            let origin = server.url();
            let read_only = ReadOnlyDrive::for_test_origin(&format!("{origin}/drive/v3/"), 4096)
                .expect("read-only test Drive");
            let transfer = TransferDrive::for_test_origins(
                &format!("{origin}/drive/v3/"),
                &format!("{origin}/upload/drive/v3/"),
                "account_1",
                "root_1",
                myvault_transfer::MAX_TRANSFER_BYTES,
            )
            .expect("transfer test Drive");

            let error = prepare_incremental_change_batch(
                &service, session_id, &mut store, &read_only, &transfer, "root_1",
            )
            .expect_err("expired cursor");
            assert_eq!(error.code, SyncCommandCode::RescanRequired);
            let state = store.vault_state().unwrap().unwrap();
            assert_eq!(state.phase, SyncPhase::NeedStartToken);
            assert!(state.rescan_required);
            assert!(state.durable_cursor.is_none());
            about.assert();
            root.assert();
            changes.assert();
        }

        #[test]
        fn incremental_download_publishes_exact_base_before_advancing_cursor() {
            const SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
            const REMOTE_REVISION: &str =
                "0000000000000000000000000000000000000000000000000000000000000002";
            const PATH: &str = "ภาษาไทย.md";

            let temporary = tempfile::tempdir().expect("temporary roots");
            let base = temporary.path().canonicalize().expect("canonical root");
            let app_data = base.join("app-data");
            let vault_root = base.join("vault");
            std::fs::create_dir(&app_data).expect("app data root");
            std::fs::create_dir(&vault_root).expect("vault root");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&app_data, std::fs::Permissions::from_mode(0o700))
                    .expect("private app data permissions");
            }
            let service = AppService::with_app_data_root(&app_data);
            let session_id = service
                .activate_trusted_vault(
                    Vault::open(vault_root.canonicalize().expect("canonical vault"))
                        .expect("open vault"),
                )
                .expect("activate vault")
                .session_id
                .expect("session");
            let context = service.native_vault_context(session_id).expect("context");
            let mut store = SyncStore::open(
                context.app_data_root().expect("app data"),
                context.vault_root(),
                context.vault_id(),
            )
            .expect("sync store");
            let binding = myvault_sync_engine::VerifiedRemoteBinding::new(
                "account_1",
                "root_1",
                "account_1",
                "root_1",
            )
            .expect("binding");
            store.bind_remote_root(&binding, 1).expect("bind");
            let mut initial = EmptyInitialDrive;
            for now in 2..=4 {
                advance_initial_sync(&mut store, &mut initial, now).expect("initial step");
            }

            let mut server = mockito::Server::new();
            let about = server
                .mock("GET", "/drive/v3/about")
                .match_query(mockito::Matcher::Any)
                .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
                .expect(4)
                .create();
            let root = server
                .mock("GET", "/drive/v3/files/root_1")
                .match_query(mockito::Matcher::Any)
                .with_body(
                    r#"{"id":"root_1","name":"Fixture","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#,
                )
                .expect(4)
                .create();
            let changes = server
                .mock("GET", "/drive/v3/changes")
                .match_query(mockito::Matcher::Any)
                .with_body(format!(
                    r#"{{"changes":[{{"fileId":"file_1","removed":false,"file":{{"id":"file_1","name":"{PATH}","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","sha256Checksum":"{SHA256}"}}}}],"newStartPageToken":"cursor_2"}}"#,
                ))
                .create();
            let remote = format!(
                r#"{{"id":"file_1","name":"{PATH}","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA256}"}}"#,
            );
            let metadata = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(mockito::Matcher::Regex("fields".into()))
                .with_body(remote)
                .expect(3)
                .create();
            let media = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(mockito::Matcher::Regex("alt=media".into()))
                .with_header("content-length", "3")
                .with_body("abc")
                .create();
            let origin = server.url();
            let read_only = ReadOnlyDrive::for_test_origin(&format!("{origin}/drive/v3/"), 4096)
                .expect("read-only test Drive");
            let transfer = TransferDrive::for_test_origins(
                &format!("{origin}/drive/v3/"),
                &format!("{origin}/upload/drive/v3/"),
                "account_1",
                "root_1",
                myvault_transfer::MAX_TRANSFER_BYTES,
            )
            .expect("transfer test Drive");

            let batch = prepare_incremental_change_batch(
                &service, session_id, &mut store, &read_only, &transfer, "root_1",
            )
            .expect("prepare incremental download");
            let mutations = store
                .local_mutations(batch.batch_id)
                .expect("durable local mutation");
            assert_eq!(mutations.len(), 1);
            assert_eq!(
                mutations[0].state,
                myvault_sync_engine::LocalMutationState::Pending
            );
            let operation_id = Uuid::parse_str(&mutations[0].mutation_id).expect("operation id");
            assert_eq!(
                store.transfer(operation_id).unwrap().unwrap().phase,
                TransferPhase::Pending
            );
            assert!(store
                .commit_transfer_change_batch(batch.batch_id, 5)
                .is_err());
            assert_eq!(
                store
                    .vault_state()
                    .unwrap()
                    .unwrap()
                    .durable_cursor
                    .as_deref(),
                Some("cursor_1")
            );
            assert!(!vault_root.join(PATH).exists());

            run_guarded_worker(&service, session_id, &mut store, transfer)
                .expect("run guarded download");

            assert_eq!(std::fs::read(vault_root.join(PATH)).unwrap(), b"abc");
            let completed = store.transfer(operation_id).unwrap().unwrap();
            let expected_base = format!("sha256-{SHA256}");
            assert_eq!(completed.phase, TransferPhase::Completed);
            assert_eq!(
                completed.base_reference.as_deref(),
                Some(expected_base.as_str())
            );
            assert_eq!(
                store.local_mutations(batch.batch_id).unwrap()[0].state,
                myvault_sync_engine::LocalMutationState::Committed
            );
            let mut sink = std::io::sink();
            let local = service
                .stream_transfer_source(
                    session_id,
                    PATH,
                    &mut sink,
                    usize::try_from(MAX_TRANSFER_BYTES).expect("transfer size fits usize"),
                )
                .expect("read back local bytes");
            let remote_base = store
                .remote_base("file_1")
                .unwrap()
                .expect("verified remote base");
            assert_eq!(remote_base.local_revision, local.revision.hex);
            assert_eq!(remote_base.remote_revision, REMOTE_REVISION);
            assert_eq!(remote_base.content_hash, SHA256);
            assert_eq!(
                store
                    .vault_state()
                    .unwrap()
                    .unwrap()
                    .durable_cursor
                    .as_deref(),
                Some("cursor_1")
            );

            store
                .commit_transfer_change_batch(batch.batch_id, now_unix_ms().unwrap())
                .expect("advance cursor after verified completion");
            assert_eq!(
                store
                    .vault_state()
                    .unwrap()
                    .unwrap()
                    .durable_cursor
                    .as_deref(),
                Some("cursor_2")
            );
            about.assert();
            root.assert();
            changes.assert();
            metadata.assert();
            media.assert();
        }
    }
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use crate::android_transfer_policy::ANDROID_MAX_TRANSFER_BYTES;
    use crate::android_transfer_runtime::{
        AndroidPrivateStoreAdapter, AndroidSafVaultIo, AndroidTransferExecutor,
    };
    use crate::app_commands::{
        android_session_id, android_vault_capability, with_android_session_lease,
        AndroidVaultSession,
    };
    use myvault_drive::{AccessToken, ReadOnlyDrive, ResolvedDriveChange, TransferDrive};
    use myvault_sync_engine::{
        advance_initial_sync, BindOutcome, RemoteChange, RemotePreviewEntry, SyncStore,
        TransferDirection, TransferMimeClass, TransferPhase, TransferRecord,
        TransferRegistrationOutcome, VaultSyncState,
    };
    use myvault_transfer::{WorkOutcome, Worker};
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{Arc, Mutex},
    };
    use tauri::Manager;
    use tauri_plugin_google_auth::{Authorization, GoogleAuthExt};
    use tauri_plugin_private_root::PrivateRootExt;
    use tauri_plugin_vault_saf::{SafEntry, SafVaultCapability, VaultSafExt};
    use unicode_normalization::UnicodeNormalization;
    use uuid::Uuid;

    const TRANSFER_PAGE_SIZE: usize = 200;
    const MAX_GUARDED_OPERATIONS: usize = 1_000;
    const MAX_INCREMENTAL_PAGES: usize = 100;
    const TRANSFER_NAMESPACE: Uuid = Uuid::from_u128(0xa9c0_4bb5_7db8_5a83_8e1c_d67a_a646_a4f2);

    #[derive(Default)]
    pub struct SyncRuntime {
        inner: Mutex<AndroidSyncState>,
    }

    #[derive(Default)]
    struct AndroidSyncState {
        account_id: Option<String>,
        root_name: Option<String>,
        authorization: Option<Authorization>,
        active: Option<AndroidActiveSync>,
        transfer_running_session: Option<VaultSessionId>,
    }

    struct AndroidActiveSync {
        session_id: VaultSessionId,
        vault_id: uuid::Uuid,
        store: SyncStore,
    }

    struct DetachedAndroidSync {
        runtime: Arc<SyncRuntime>,
        active: Option<AndroidActiveSync>,
    }

    impl DetachedAndroidSync {
        fn store_mut(&mut self) -> Result<&mut SyncStore, SyncCommandError> {
            self.active
                .as_mut()
                .map(|active| &mut active.store)
                .ok_or_else(SyncCommandError::internal)
        }

        fn discard_store(&mut self) -> Result<(), SyncCommandError> {
            let session_id = self
                .active
                .as_ref()
                .map(|active| active.session_id)
                .ok_or_else(SyncCommandError::internal)?;
            drop(self.active.take());
            let mut state = self
                .runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if state.active.is_some() || state.transfer_running_session != Some(session_id) {
                return Err(SyncCommandError::internal());
            }
            state.transfer_running_session = None;
            Ok(())
        }

        fn restore_inner(&mut self) -> Result<(), SyncCommandError> {
            let Some(active) = self.active.take() else {
                return Ok(());
            };
            let mut state = self
                .runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if state.active.is_some() || state.transfer_running_session != Some(active.session_id) {
                return Err(SyncCommandError::internal());
            }
            state.active = Some(active);
            state.transfer_running_session = None;
            Ok(())
        }
    }

    impl Drop for DetachedAndroidSync {
        fn drop(&mut self) {
            let _ = self.restore_inner();
        }
    }

    fn validate_session(
        session: &AndroidVaultSession,
        session_id: &str,
    ) -> Result<VaultSessionId, SyncCommandError> {
        let requested = parse_session_id(session_id)?;
        android_session_id(session, requested).map_err(map_app_error)
    }

    fn with_current_session<T>(
        app: &tauri::AppHandle,
        session_id: VaultSessionId,
        vault: &SafVaultCapability,
        operation: impl FnOnce() -> Result<T, SyncCommandError>,
    ) -> Result<T, SyncCommandError> {
        let session = app.state::<AndroidVaultSession>();
        let current = android_vault_capability(&session, session_id).map_err(map_app_error)?;
        if current != *vault {
            return Err(SyncCommandError::new(
                SyncCommandCode::StaleSession,
                "the local vault session is stale",
            ));
        }
        with_android_session_lease(&session, session_id, operation).map_err(map_app_error)?
    }

    fn ensure_store<'a>(
        app: &tauri::AppHandle,
        state: &'a mut AndroidSyncState,
        session_id: VaultSessionId,
        vault: &SafVaultCapability,
    ) -> Result<&'a mut SyncStore, SyncCommandError> {
        let vault_id = vault.vault_id();
        let matches = state
            .active
            .as_ref()
            .is_some_and(|active| active.session_id == session_id && active.vault_id == vault_id);
        if !matches {
            if state.transfer_running_session.is_some() {
                return Err(SyncCommandError::new(
                    SyncCommandCode::Busy,
                    "a guarded transfer is already running",
                ));
            }
            state.active = None;
            let root = app.native_no_backup_root().map_err(|_| {
                SyncCommandError::new(
                    SyncCommandCode::StorageUnavailable,
                    "private sync state is unavailable",
                )
            })?;
            let store = root.open_sync_store(vault_id).map_err(|_| {
                SyncCommandError::new(
                    SyncCommandCode::StorageUnavailable,
                    "private sync state is unavailable",
                )
            })?;
            state.active = Some(AndroidActiveSync {
                session_id,
                vault_id,
                store,
            });
            state.root_name = None;
        }
        state
            .active
            .as_mut()
            .map(|active| &mut active.store)
            .ok_or_else(SyncCommandError::internal)
    }

    fn android_status(
        session_id: VaultSessionId,
        state: &AndroidSyncState,
        persisted: Option<&VaultSyncState>,
        transfers: TransferSummary,
    ) -> SyncStatusDto {
        let bound = persisted.is_some_and(|value| value.account_id.is_some());
        let account_id = persisted
            .and_then(|value| value.account_id.clone())
            .or_else(|| state.account_id.clone());
        SyncStatusDto {
            session_id,
            supported: true,
            binding_available: true,
            configured: true,
            connected: state.authorization.is_some(),
            bound,
            account_id,
            root_id: persisted.map(|value| value.remote_root_id.clone()),
            root_name: state.root_name.clone(),
            phase: persisted.map_or("unbound", |value| phase_name(value.phase)),
            rescan_required: persisted.is_some_and(|value| value.rescan_required),
            active: transfers.active(),
            pending: transfers.pending,
            retry_scheduled: transfers.retry_scheduled,
            auth_required: transfers.auth_required,
            needs_reconcile: transfers.needs_reconcile,
            completed: transfers.completed,
        }
    }

    fn drive_from(authorization: &Authorization) -> Result<ReadOnlyDrive, SyncCommandError> {
        authorization
            .with_native_access_token(|token| {
                ReadOnlyDrive::google(AccessToken::new(token.to_owned()))
            })
            .map_err(map_drive_error)
    }

    fn exact_account(expected: Option<&str>, observed: &str) -> Result<(), SyncCommandError> {
        if expected.is_none() || expected == Some(observed) {
            Ok(())
        } else {
            Err(SyncCommandError::new(
                SyncCommandCode::BindingMismatch,
                "the Google account does not match this Vault's exact binding",
            ))
        }
    }

    fn detach_store(
        app: &tauri::AppHandle,
        runtime: Arc<SyncRuntime>,
        session_id: VaultSessionId,
        vault: &SafVaultCapability,
    ) -> Result<DetachedAndroidSync, SyncCommandError> {
        let active = {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            ensure_store(app, &mut state, session_id, vault)?;
            let active = state.active.take().ok_or_else(SyncCommandError::internal)?;
            state.transfer_running_session = Some(session_id);
            active
        };
        Ok(DetachedAndroidSync {
            runtime,
            active: Some(active),
        })
    }

    fn transfer_drive_from(
        authorization: &Authorization,
        account_id: &str,
        root_id: &str,
    ) -> Result<TransferDrive, SyncCommandError> {
        authorization
            .with_native_access_token(|token| {
                TransferDrive::google(AccessToken::new(token.to_owned()), account_id, root_id)
            })
            .map_err(map_drive_error)
    }

    fn transfer_operation_id(parts: &[&str]) -> Uuid {
        let mut evidence = String::from("myvault-r2\0");
        for part in parts {
            evidence.push_str(&part.len().to_string());
            evidence.push(':');
            evidence.push_str(part);
            evidence.push('\0');
        }
        Uuid::new_v5(&TRANSFER_NAMESPACE, evidence.as_bytes())
    }

    fn stage_reference(operation_id: Uuid) -> String {
        format!("stage-{operation_id}")
    }

    fn operation_marker(operation_id: Uuid) -> String {
        format!("r2-{}", operation_id.simple())
    }

    fn content_kind(path: &str, markdown: bool) -> TransferMimeClass {
        if markdown
            || path
                .rsplit_once('.')
                .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("md"))
        {
            TransferMimeClass::Markdown
        } else {
            TransferMimeClass::Blob
        }
    }

    fn parent_path(path: &str) -> Option<&str> {
        path.rsplit_once('/').map(|(parent, _)| parent)
    }

    fn display_name(path: &str) -> Result<&str, SyncCommandError> {
        path.rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "a transfer path is invalid",
                )
            })
    }

    fn collision_key(path: &str) -> String {
        path.nfc().flat_map(char::to_lowercase).collect()
    }

    fn reject_portable_path_collisions<'a>(
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), SyncCommandError> {
        let mut seen = BTreeMap::new();
        for path in paths {
            let key = collision_key(path);
            if seen
                .insert(key, path)
                .is_some_and(|existing| existing != path)
            {
                return Err(SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "case-folded or Unicode-normalized transfer paths collide",
                ));
            }
        }
        Ok(())
    }

    fn collect_remote_preview(
        store: &SyncStore,
    ) -> Result<Vec<RemotePreviewEntry>, SyncCommandError> {
        let mut cursor = None;
        let mut entries = Vec::new();
        loop {
            let page = store
                .remote_preview(cursor.as_ref(), TRANSFER_PAGE_SIZE)
                .map_err(map_sync_error)?;
            if page.rescan_required || page.colliding_entries != 0 {
                return Err(SyncCommandError::new(
                    SyncCommandCode::RescanRequired,
                    "Drive metadata must be collision-free and freshly scanned before transfer",
                ));
            }
            if entries.len().saturating_add(page.entries.len()) > MAX_GUARDED_OPERATIONS * 2 {
                return Err(SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "the guarded transfer plan exceeds the operation limit",
                ));
            }
            entries.extend(page.entries);
            if !page.has_more {
                break;
            }
            cursor = page.next_after;
            if cursor.is_none() {
                return Err(SyncCommandError::internal());
            }
        }
        Ok(entries)
    }

    fn android_inventory(
        app: &tauri::AppHandle,
        vault: &SafVaultCapability,
    ) -> Result<Vec<SafEntry>, SyncCommandError> {
        let mut inventory = app.vault_saf().inventory(vault).map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::StorageUnavailable,
                "exact Android Vault inventory is unavailable",
            )
        })?;
        inventory.normalize_portable_order();
        if inventory.entries.len() > MAX_GUARDED_OPERATIONS {
            return Err(SyncCommandError::new(
                SyncCommandCode::InvalidRequest,
                "the guarded transfer plan exceeds the operation limit",
            ));
        }
        Ok(inventory.entries)
    }

    fn register_or_reconcile(
        store: &mut SyncStore,
        record: &TransferRecord,
        reconciliation_time: u64,
        local_absence_proven: bool,
    ) -> Result<bool, SyncCommandError> {
        match store.register_transfer(record).map_err(map_sync_error)? {
            TransferRegistrationOutcome::Registered => Ok(true),
            TransferRegistrationOutcome::AlreadyCompleted => Ok(false),
            TransferRegistrationOutcome::AlreadyPresent => {
                let existing = store
                    .transfer(record.operation_id)
                    .map_err(map_sync_error)?
                    .ok_or_else(SyncCommandError::internal)?;
                if existing.phase == TransferPhase::NeedsReconcile
                    && (local_absence_proven
                        || existing.last_error_code.as_deref()
                            != Some("android_local_publication_unknown"))
                {
                    store
                        .requeue_transfer_for_reconciliation(
                            record.operation_id,
                            reconciliation_time.max(existing.updated_at_unix_ms),
                        )
                        .map_err(map_sync_error)?;
                }
                Ok(false)
            }
        }
    }

    fn prepare_guarded_transfers(
        app: &tauri::AppHandle,
        vault: &SafVaultCapability,
        store: &mut SyncStore,
        drive: &TransferDrive,
        root_id: &str,
    ) -> Result<u64, SyncCommandError> {
        let remote_entries = collect_remote_preview(store)?;
        let local_entries = android_inventory(app, vault)?;
        reject_portable_path_collisions(
            remote_entries
                .iter()
                .map(|entry| entry.path.as_str())
                .chain(local_entries.iter().map(|entry| entry.path.as_str())),
        )?;
        let local_paths = local_entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<BTreeSet<_>>();
        let remote_by_path = remote_entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect::<BTreeMap<_, _>>();
        let remote_folders = remote_entries
            .iter()
            .filter(|entry| entry.kind == RemoteEntryKind::Folder)
            .map(|entry| (entry.path.as_str(), entry.file_id.as_str()))
            .collect::<BTreeMap<_, _>>();
        let mut registered = 0_u64;
        let reconciliation_time = now_unix_ms()?;

        for local in local_entries {
            if local.byte_len_known && local.byte_len > ANDROID_MAX_TRANSFER_BYTES as u64 {
                return Err(SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "a local file exceeds the guarded transfer size limit",
                ));
            }
            let binary = app
                .vault_saf()
                .read_binary(vault, &local.path, ANDROID_MAX_TRANSFER_BYTES)
                .map_err(|_| {
                    SyncCommandError::new(
                        SyncCommandCode::StorageUnavailable,
                        "exact local transfer evidence is unavailable",
                    )
                })?;
            let digest = myvault_core::Sha256Digest::from_bytes(&binary.bytes);
            let revision = myvault_core::FileRevision::from_bytes(&binary.bytes);
            if (local.byte_len_known && binary.byte_len != local.byte_len)
                || binary.revision_hex != digest.as_str()
            {
                return Err(SyncCommandError::new(
                    SyncCommandCode::StorageUnavailable,
                    "exact local transfer evidence is unavailable",
                ));
            }
            let matching_remote = remote_by_path.get(local.path.as_str()).copied();
            let matching_durable = matching_remote
                .map(|entry| {
                    store
                        .remote_entry(&entry.file_id)
                        .map_err(map_sync_error)?
                        .ok_or_else(SyncCommandError::internal)
                })
                .transpose()?;
            let matching_base = matching_remote
                .map(|entry| store.remote_base(&entry.file_id).map_err(map_sync_error))
                .transpose()?
                .flatten();
            let exact_base_match = matching_durable.as_ref().is_some_and(|durable| {
                matching_base.as_ref().is_some_and(|base| {
                    base.local_revision == revision.hex
                        && base.remote_revision == durable.remote_revision
                        && base.content_hash == digest.as_str()
                })
            });
            if exact_base_match {
                let remote = matching_remote.ok_or_else(SyncCommandError::internal)?;
                let operation_id = transfer_operation_id(&[
                    "upload",
                    &local.path,
                    &remote.parent_id,
                    "absent",
                    "absent",
                    &revision.hex,
                    digest.as_str(),
                    &binary.byte_len.to_string(),
                ]);
                if requeue_prior_absent_upload_if_exact(
                    store,
                    operation_id,
                    &local.path,
                    &remote.parent_id,
                    &revision.hex,
                    digest.as_str(),
                    binary.byte_len,
                    content_kind(&local.path, local.kind == "markdown"),
                    reconciliation_time,
                    Some("android_local_publication_unknown"),
                )? {
                    registered = registered.saturating_add(1);
                }
                continue;
            }
            let parent_id = if let Some(remote) = matching_remote {
                remote.parent_id.as_str()
            } else if let Some(parent) = parent_path(&local.path) {
                remote_folders.get(parent).copied().ok_or_else(|| {
                    SyncCommandError::new(
                        SyncCommandCode::InvalidRequest,
                        "a required remote parent folder does not exist",
                    )
                })?
            } else {
                root_id
            };
            let remote_identity = matching_remote.map_or("absent", |entry| entry.file_id.as_str());
            let remote_revision = matching_durable
                .as_ref()
                .map_or("absent", |entry| entry.remote_revision.as_str());
            let operation_id = transfer_operation_id(&[
                "upload",
                &local.path,
                parent_id,
                remote_identity,
                remote_revision,
                &revision.hex,
                digest.as_str(),
                &binary.byte_len.to_string(),
            ]);
            let record = TransferRecord::new(
                operation_id,
                TransferDirection::Upload,
                local.path.clone(),
                parent_id,
                matching_remote.map(|entry| entry.file_id.clone()),
                Some(revision.hex),
                matching_durable.map(|entry| entry.remote_revision),
                digest.as_str(),
                binary.byte_len,
                content_kind(&local.path, local.kind == "markdown"),
                operation_marker(operation_id),
                Some(stage_reference(operation_id)),
                None,
                0,
            )
            .map_err(map_sync_error)?;
            if register_or_reconcile(store, &record, reconciliation_time, false)? {
                registered = registered.saturating_add(1);
            }
        }

        for remote in remote_entries
            .iter()
            .filter(|entry| entry.kind == RemoteEntryKind::File)
            .filter(|entry| !local_paths.contains(entry.path.as_str()))
        {
            let durable = store
                .remote_entry(&remote.file_id)
                .map_err(map_sync_error)?
                .ok_or_else(SyncCommandError::internal)?;
            let candidate = drive
                .inspect_download_candidate(
                    &remote.file_id,
                    &remote.parent_id,
                    display_name(&remote.path)?,
                    &durable.remote_revision,
                )
                .map_err(map_drive_error)?;
            if candidate.size() > ANDROID_MAX_TRANSFER_BYTES as u64 {
                return Err(SyncCommandError::new(
                    SyncCommandCode::InvalidRequest,
                    "a remote file exceeds the Android guarded transfer size limit",
                ));
            }
            let operation_id = transfer_operation_id(&[
                "download",
                &remote.path,
                &remote.parent_id,
                candidate.file_id(),
                candidate.sync_revision(),
                candidate.sha256(),
                &candidate.size().to_string(),
            ]);
            let record = TransferRecord::new(
                operation_id,
                TransferDirection::Download,
                remote.path.clone(),
                remote.parent_id.clone(),
                Some(remote.file_id.clone()),
                None,
                Some(candidate.sync_revision().to_owned()),
                candidate.sha256(),
                candidate.size(),
                content_kind(&remote.path, false),
                operation_marker(operation_id),
                Some(stage_reference(operation_id)),
                None,
                0,
            )
            .map_err(map_sync_error)?;
            // This record is created only from the exact current SAF inventory
            // branch where the portable path is absent. A prior publication-
            // unknown attempt can therefore be retried without replacing an
            // existing local document.
            if register_or_reconcile(store, &record, reconciliation_time, true)? {
                registered = registered.saturating_add(1);
            }
        }
        Ok(registered)
    }

    struct IncrementalBatch {
        batch_id: Uuid,
        final_page: bool,
    }

    fn prepare_incremental_change_batch(
        app: &tauri::AppHandle,
        vault: &SafVaultCapability,
        store: &mut SyncStore,
        read_only: &ReadOnlyDrive,
        transfer_drive: &TransferDrive,
        root_id: &str,
    ) -> Result<IncrementalBatch, SyncCommandError> {
        let state = store
            .vault_state()
            .map_err(map_sync_error)?
            .ok_or_else(SyncCommandError::internal)?;
        let account_id = state.account_id.as_deref().ok_or_else(|| {
            SyncCommandError::new(
                SyncCommandCode::BindingMismatch,
                "an exact Drive account must be bound before transfer",
            )
        })?;
        let binding = read_only
            .verify_binding(account_id, root_id)
            .map_err(map_drive_error)?;
        store
            .verify_remote_binding(&binding)
            .map_err(map_sync_error)?;
        let cursor = state.durable_cursor.as_deref().ok_or_else(|| {
            SyncCommandError::new(
                SyncCommandCode::RescanRequired,
                "Drive metadata must have a durable cursor before transfer",
            )
        })?;
        let page = map_drive_with_rescan(store, read_only.changes_page(cursor))?;
        let next_cursor = page
            .next_page_token
            .as_deref()
            .or(page.new_start_page_token.as_deref())
            .ok_or_else(SyncCommandError::internal)?;
        let final_page = page.new_start_page_token.is_some();
        let mut changes = Vec::new();
        let mut downloads = Vec::new();
        let mut merged_remote_paths = collect_remote_preview(store)?
            .into_iter()
            .map(|entry| (entry.file_id, entry.path))
            .collect::<BTreeMap<_, _>>();

        for raw in &page.changes {
            let known = store.remote_entry(&raw.file_id).map_err(map_sync_error)?;
            match map_drive_with_rescan(store, read_only.resolve_change_below_root(root_id, raw))? {
                ResolvedDriveChange::Removed { .. } | ResolvedDriveChange::OutsideBoundRoot => {
                    if known.is_some() {
                        store
                            .mark_rescan_required(now_unix_ms()?)
                            .map_err(map_sync_error)?;
                        return Err(SyncCommandError::new(
                            SyncCommandCode::RescanRequired,
                            "remote move or removal requires explicit reconciliation",
                        ));
                    }
                }
                ResolvedDriveChange::Inside(entry) => {
                    if !myvault_sync_engine::is_valid_sync_content_path(&entry.path) {
                        store
                            .mark_rescan_required(now_unix_ms()?)
                            .map_err(map_sync_error)?;
                        return Err(SyncCommandError::new(
                            SyncCommandCode::RescanRequired,
                            "protected or invalid remote paths require explicit reconciliation",
                        ));
                    }
                    if let Some(previous) = known.as_ref() {
                        if previous.path != entry.path
                            || previous.parent_id != entry.parent_id
                            || previous.kind != entry.kind
                        {
                            store
                                .mark_rescan_required(now_unix_ms()?)
                                .map_err(map_sync_error)?;
                            return Err(SyncCommandError::new(
                                SyncCommandCode::RescanRequired,
                                "remote move or rename requires explicit reconciliation",
                            ));
                        }
                    }
                    merged_remote_paths.insert(entry.file_id.clone(), entry.path.clone());
                    let requires_download = entry.kind == RemoteEntryKind::File
                        && known.as_ref().is_none_or(|previous| {
                            previous.remote_revision != entry.remote_revision
                                || previous.content_hash != entry.content_hash
                        });
                    if requires_download {
                        let candidate = transfer_drive
                            .inspect_download_candidate(
                                &entry.file_id,
                                &entry.parent_id,
                                display_name(&entry.path)?,
                                &entry.remote_revision,
                            )
                            .map_err(map_drive_error)?;
                        if candidate.size() > ANDROID_MAX_TRANSFER_BYTES as u64 {
                            return Err(SyncCommandError::new(
                                SyncCommandCode::InvalidRequest,
                                "a remote file exceeds the Android guarded transfer size limit",
                            ));
                        }
                        let operation_id = transfer_operation_id(&[
                            "download",
                            &entry.path,
                            &entry.parent_id,
                            candidate.file_id(),
                            candidate.sync_revision(),
                            candidate.sha256(),
                            &candidate.size().to_string(),
                        ]);
                        downloads.push(
                            TransferRecord::new(
                                operation_id,
                                TransferDirection::Download,
                                entry.path.clone(),
                                entry.parent_id.clone(),
                                Some(entry.file_id.clone()),
                                None,
                                Some(candidate.sync_revision().to_owned()),
                                candidate.sha256(),
                                candidate.size(),
                                content_kind(&entry.path, false),
                                operation_marker(operation_id),
                                Some(stage_reference(operation_id)),
                                None,
                                0,
                            )
                            .map_err(map_sync_error)?,
                        );
                    }
                    changes.push(RemoteChange::Upsert(entry));
                }
            }
        }
        let local = android_inventory(app, vault)?;
        let mut exact_remote_paths = BTreeSet::new();
        if merged_remote_paths
            .values()
            .any(|path| !exact_remote_paths.insert(path.as_str()))
        {
            store
                .mark_rescan_required(now_unix_ms()?)
                .map_err(map_sync_error)?;
            return Err(SyncCommandError::new(
                SyncCommandCode::RescanRequired,
                "duplicate remote paths require explicit reconciliation",
            ));
        }
        reject_portable_path_collisions(
            merged_remote_paths
                .values()
                .map(String::as_str)
                .chain(local.iter().map(|entry| entry.path.as_str())),
        )?;
        let batch_id = transfer_operation_id(&["changes", cursor, next_cursor]);
        store
            .begin_transfer_change_batch(batch_id, cursor, next_cursor, &changes, &downloads)
            .map_err(map_sync_error)?;
        Ok(IncrementalBatch {
            batch_id,
            final_page,
        })
    }

    fn requeue_active_batch_reconciliation(
        store: &mut SyncStore,
        batch_id: Uuid,
        now: u64,
    ) -> Result<(), SyncCommandError> {
        for mutation in store.local_mutations(batch_id).map_err(map_sync_error)? {
            let operation_id =
                Uuid::parse_str(&mutation.mutation_id).map_err(|_| SyncCommandError::internal())?;
            let transfer = store
                .transfer(operation_id)
                .map_err(map_sync_error)?
                .ok_or_else(SyncCommandError::internal)?;
            if transfer.phase == TransferPhase::NeedsReconcile
                && transfer.last_error_code.as_deref() != Some("android_local_publication_unknown")
            {
                store
                    .requeue_transfer_for_reconciliation(
                        operation_id,
                        now.max(transfer.updated_at_unix_ms),
                    )
                    .map_err(map_sync_error)?;
            }
        }
        Ok(())
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum GuardedWorkerOutcome {
        Drained,
        AuthRequired,
    }

    fn run_guarded_worker(
        app: &tauri::AppHandle,
        vault: &SafVaultCapability,
        store: &mut SyncStore,
        drive: TransferDrive,
    ) -> Result<GuardedWorkerOutcome, SyncCommandError> {
        let private = app
            .native_no_backup_root()
            .map_err(|_| {
                SyncCommandError::new(
                    SyncCommandCode::StorageUnavailable,
                    "private transfer storage is unavailable",
                )
            })?
            .open_transfer_store(vault.vault_id())
            .map_err(|_| {
                SyncCommandError::new(
                    SyncCommandCode::StorageUnavailable,
                    "private transfer storage is unavailable",
                )
            })?;
        let executor = AndroidTransferExecutor::new(
            AndroidSafVaultIo::new(app.clone(), vault.clone()),
            AndroidPrivateStoreAdapter::new(private),
            drive,
        );
        let mut worker = Worker::new(store, executor);
        for _ in 0..MAX_GUARDED_OPERATIONS {
            match worker
                .run_once(now_unix_ms()?)
                .map_err(|_| SyncCommandError::internal())?
            {
                WorkOutcome::Idle => return Ok(GuardedWorkerOutcome::Drained),
                WorkOutcome::Completed(_)
                | WorkOutcome::RetryScheduled(_)
                | WorkOutcome::NeedsReconcile(_) => {}
                WorkOutcome::AuthRequired(_) => {
                    return Ok(GuardedWorkerOutcome::AuthRequired);
                }
            }
        }
        Ok(GuardedWorkerOutcome::Drained)
    }

    fn refresh_run_authorization_once(
        app: &tauri::AppHandle,
        expected_account_id: &str,
        authorization: &mut Authorization,
        refresh_attempted: &mut bool,
    ) -> Result<bool, SyncCommandError> {
        if *refresh_attempted {
            return Ok(false);
        }
        *refresh_attempted = true;
        let refreshed = app.google_auth().fresh_access_token().map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::AuthRequired,
                "Google authorization could not be refreshed",
            )
        })?;
        let observed = drive_from(&refreshed)?
            .account_identity()
            .map_err(map_drive_error)?;
        exact_account(Some(expected_account_id), &observed.permission_id)?;
        *authorization = refreshed;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_guarded_worker_with_auth_refresh(
        app: &tauri::AppHandle,
        vault: &SafVaultCapability,
        store: &mut SyncStore,
        account_id: &str,
        root_id: &str,
        authorization: &mut Authorization,
        refresh_attempted: &mut bool,
    ) -> Result<GuardedWorkerOutcome, SyncCommandError> {
        let first = run_guarded_worker(
            app,
            vault,
            store,
            transfer_drive_from(authorization, account_id, root_id)?,
        )?;
        if first != GuardedWorkerOutcome::AuthRequired
            || !refresh_run_authorization_once(app, account_id, authorization, refresh_attempted)?
        {
            return Ok(first);
        }
        store
            .resume_auth_required_transfers(now_unix_ms()?)
            .map_err(map_sync_error)?;
        run_guarded_worker(
            app,
            vault,
            store,
            transfer_drive_from(authorization, account_id, root_id)?,
        )
    }

    #[tauri::command(rename_all = "camelCase")]
    pub fn sync_status(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let session_id = validate_session(&session, &session_id)?;
        let vault = android_vault_capability(&session, session_id).map_err(map_app_error)?;
        with_android_session_lease(&session, session_id, || {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            let (persisted, transfers) = {
                let store = ensure_store(&app, &mut state, session_id, &vault)?;
                (
                    store.vault_state().map_err(map_sync_error)?,
                    store.transfer_summary().map_err(map_sync_error)?,
                )
            };
            Ok(android_status(
                session_id,
                &state,
                persisted.as_ref(),
                transfers,
            ))
        })
        .map_err(map_app_error)?
    }

    fn sync_connect_impl(
        app: tauri::AppHandle,
        runtime: Arc<SyncRuntime>,
        requested: VaultSessionId,
        vault: SafVaultCapability,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let persisted_account = {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            ensure_store(&app, &mut state, requested, &vault)?
                .vault_state()
                .map_err(map_sync_error)?
                .and_then(|value| value.account_id)
        };
        let authorization = app.google_auth().fresh_access_token().map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::AuthRequired,
                "Google authorization could not be completed",
            )
        })?;
        let drive = drive_from(&authorization)?;
        let account = drive.account_identity().map_err(map_drive_error)?;
        exact_account(persisted_account.as_deref(), &account.permission_id)?;
        let finalized = with_current_session(&app, requested, &vault, || {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            let (persisted, transfers) = {
                let store = ensure_store(&app, &mut state, requested, &vault)?;
                (
                    store.vault_state().map_err(map_sync_error)?,
                    store.transfer_summary().map_err(map_sync_error)?,
                )
            };
            state.account_id = Some(account.permission_id);
            state.authorization = Some(authorization);
            Ok(android_status(
                requested,
                &state,
                persisted.as_ref(),
                transfers,
            ))
        });
        if finalized.is_err() {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if state.transfer_running_session.is_none()
                && state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.session_id == requested)
            {
                state.active = None;
                state.root_name = None;
            }
        }
        finalized
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_connect(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let requested = parse_session_id(&session_id)?;
        let vault = android_vault_capability(&session, requested).map_err(map_app_error)?;
        let runtime = Arc::clone(runtime.inner());
        let result = tauri::async_runtime::spawn_blocking(move || {
            sync_connect_impl(app, runtime, requested, vault)
        })
        .await
        .map_err(|_| SyncCommandError::internal())??;
        android_vault_capability(&session, requested).map_err(map_app_error)?;
        Ok(result)
    }

    fn sync_list_folders_impl(
        app: tauri::AppHandle,
        runtime: Arc<SyncRuntime>,
        requested: VaultSessionId,
        vault: SafVaultCapability,
        parent_id: Option<String>,
        page_token: Option<String>,
    ) -> Result<RemoteFolderPageDto, SyncCommandError> {
        let authorization = app.google_auth().fresh_access_token().map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::AuthRequired,
                "Google authorization could not be refreshed",
            )
        })?;
        let drive = drive_from(&authorization)?;
        let account = drive.account_identity().map_err(map_drive_error)?;
        let expected_account_id = runtime
            .inner
            .lock()
            .map_err(|_| SyncCommandError::internal())?
            .account_id
            .clone()
            .ok_or_else(|| {
                SyncCommandError::new(
                    SyncCommandCode::AuthRequired,
                    "Google authorization is required",
                )
            })?;
        exact_account(Some(&expected_account_id), &account.permission_id)?;
        let page = drive
            .list_children_page(
                parent_id.as_deref().unwrap_or("root"),
                page_token.as_deref(),
            )
            .map_err(map_drive_error)?;
        with_current_session(&app, requested, &vault, || {
            runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?
                .authorization = Some(authorization);
            Ok(folder_page(requested, parent_id, page))
        })
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_list_folders(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
        parent_id: Option<String>,
        page_token: Option<String>,
    ) -> Result<RemoteFolderPageDto, SyncCommandError> {
        let requested = parse_session_id(&session_id)?;
        let vault = android_vault_capability(&session, requested).map_err(map_app_error)?;
        let runtime = Arc::clone(runtime.inner());
        let result = tauri::async_runtime::spawn_blocking(move || {
            sync_list_folders_impl(app, runtime, requested, vault, parent_id, page_token)
        })
        .await
        .map_err(|_| SyncCommandError::internal())??;
        android_vault_capability(&session, requested).map_err(map_app_error)?;
        Ok(result)
    }

    fn sync_bind_root_impl(
        app: tauri::AppHandle,
        runtime: Arc<SyncRuntime>,
        session_id: VaultSessionId,
        vault: SafVaultCapability,
        account_id: String,
        root_id: String,
    ) -> Result<BindRootDto, SyncCommandError> {
        let authorization = app.google_auth().fresh_access_token().map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::AuthRequired,
                "Google authorization could not be refreshed",
            )
        })?;
        let drive = drive_from(&authorization)?;
        let observed = drive.account_identity().map_err(map_drive_error)?;
        exact_account(Some(&account_id), &observed.permission_id)?;
        let root = drive.verify_root(&root_id).map_err(map_drive_error)?;
        let binding = drive
            .verify_binding(&account_id, &root_id)
            .map_err(map_drive_error)?;
        with_current_session(&app, session_id, &vault, || {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            exact_account(state.account_id.as_deref(), &observed.permission_id)?;
            let (outcome, persisted, transfers) = {
                let store = ensure_store(&app, &mut state, session_id, &vault)?;
                let outcome = store
                    .bind_remote_root(&binding, now_unix_ms()?)
                    .map_err(map_sync_error)?;
                (
                    outcome,
                    store.vault_state().map_err(map_sync_error)?,
                    store.transfer_summary().map_err(map_sync_error)?,
                )
            };
            state.account_id = Some(observed.permission_id);
            state.authorization = Some(authorization);
            state.root_name = Some(root.name);
            Ok(BindRootDto {
                session_id,
                outcome: match outcome {
                    BindOutcome::Created => "created",
                    BindOutcome::AlreadyBound => "alreadyBound",
                    BindOutcome::LegacyBindingConfirmed => "legacyBindingConfirmed",
                },
                status: android_status(session_id, &state, persisted.as_ref(), transfers),
            })
        })
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_bind_root(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
        account_id: String,
        root_id: String,
    ) -> Result<BindRootDto, SyncCommandError> {
        let session_id = validate_session(&session, &session_id)?;
        let vault = android_vault_capability(&session, session_id).map_err(map_app_error)?;
        let runtime = Arc::clone(runtime.inner());
        let result = tauri::async_runtime::spawn_blocking(move || {
            sync_bind_root_impl(app, runtime, session_id, vault, account_id, root_id)
        })
        .await
        .map_err(|_| SyncCommandError::internal())??;
        android_vault_capability(&session, session_id).map_err(map_app_error)?;
        Ok(result)
    }

    fn sync_scan_step_impl(
        app: tauri::AppHandle,
        runtime: Arc<SyncRuntime>,
        session_id: VaultSessionId,
        vault: SafVaultCapability,
    ) -> Result<ScanStepDto, SyncCommandError> {
        let account_id = {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            let account_id = ensure_store(&app, &mut state, session_id, &vault)?
                .vault_state()
                .map_err(map_sync_error)?
                .and_then(|value| value.account_id)
                .ok_or_else(|| {
                    SyncCommandError::new(
                        SyncCommandCode::BindingMismatch,
                        "an exact Drive root must be bound before scanning",
                    )
                })?;
            if state.authorization.is_none() {
                return Err(SyncCommandError::new(
                    SyncCommandCode::AuthRequired,
                    "Google authorization is required",
                ));
            }
            account_id
        };
        let authorization = app.google_auth().fresh_access_token().map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::AuthRequired,
                "Google authorization could not be refreshed",
            )
        })?;
        let mut drive = drive_from(&authorization)?;
        let observed = drive.account_identity().map_err(map_drive_error)?;
        exact_account(Some(&account_id), &observed.permission_id)?;
        let mut detached = detach_store(&app, Arc::clone(&runtime), session_id, &vault)?;
        let root_id = detached
            .store_mut()?
            .vault_state()
            .map_err(map_sync_error)?
            .ok_or_else(SyncCommandError::internal)?
            .remote_root_id;
        let binding = drive
            .verify_binding(&account_id, &root_id)
            .map_err(map_drive_error)?;
        detached
            .store_mut()?
            .verify_remote_binding(&binding)
            .map_err(map_sync_error)?;
        let progress = advance_initial_sync(detached.store_mut()?, &mut drive, now_unix_ms()?)
            .map_err(map_sync_error)?;
        let mut restored = false;
        let finalized = with_current_session(&app, session_id, &vault, || {
            detached.restore_inner()?;
            restored = true;
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            state.authorization = Some(authorization);
            let (persisted, transfers) = {
                let store = ensure_store(&app, &mut state, session_id, &vault)?;
                (
                    store.vault_state().map_err(map_sync_error)?,
                    store.transfer_summary().map_err(map_sync_error)?,
                )
            };
            Ok(ScanStepDto {
                session_id,
                progress: progress_name(progress),
                status: android_status(session_id, &state, persisted.as_ref(), transfers),
            })
        });
        match finalized {
            Err(error) if !restored => {
                detached.discard_store()?;
                Err(error)
            }
            result => result,
        }
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_scan_step(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<ScanStepDto, SyncCommandError> {
        let session_id = validate_session(&session, &session_id)?;
        let vault = android_vault_capability(&session, session_id).map_err(map_app_error)?;
        let runtime = Arc::clone(runtime.inner());
        let result = tauri::async_runtime::spawn_blocking(move || {
            sync_scan_step_impl(app, runtime, session_id, vault)
        })
        .await
        .map_err(|_| SyncCommandError::internal())??;
        android_vault_capability(&session, session_id).map_err(map_app_error)?;
        Ok(result)
    }

    #[tauri::command(rename_all = "camelCase")]
    pub fn sync_preview(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
        after: Option<String>,
        limit: Option<usize>,
    ) -> Result<RemotePreviewPageDto, SyncCommandError> {
        let session_id = validate_session(&session, &session_id)?;
        let vault = android_vault_capability(&session, session_id).map_err(map_app_error)?;
        let cursor = decode_preview_cursor(after.as_deref())?;
        let limit = limit.unwrap_or(DEFAULT_PREVIEW_LIMIT);
        let mut state = runtime
            .inner
            .lock()
            .map_err(|_| SyncCommandError::internal())?;
        let page = ensure_store(&app, &mut state, session_id, &vault)?
            .remote_preview(cursor.as_ref(), limit)
            .map_err(map_sync_error)?;
        preview_page(session_id, page)
    }

    fn sync_run_guarded_impl(
        app: tauri::AppHandle,
        runtime: Arc<SyncRuntime>,
        session_id: VaultSessionId,
        vault: SafVaultCapability,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let bound = {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if state.authorization.is_none() {
                return Err(SyncCommandError::new(
                    SyncCommandCode::AuthRequired,
                    "Google authorization is required",
                ));
            }
            let bound = ensure_store(&app, &mut state, session_id, &vault)?
                .vault_state()
                .map_err(map_sync_error)?
                .ok_or_else(|| {
                    SyncCommandError::new(
                        SyncCommandCode::BindingMismatch,
                        "an exact Drive root must be bound before transfer",
                    )
                })?;
            if bound.phase != SyncPhase::Ready || bound.rescan_required {
                return Err(SyncCommandError::new(
                    SyncCommandCode::RescanRequired,
                    "Drive metadata must be fully scanned before transfer",
                ));
            }
            bound
        };
        let account_id = bound.account_id.clone().ok_or_else(|| {
            SyncCommandError::new(
                SyncCommandCode::BindingMismatch,
                "an exact Drive account must be bound before transfer",
            )
        })?;
        let mut authorization = app.google_auth().fresh_access_token().map_err(|_| {
            SyncCommandError::new(
                SyncCommandCode::AuthRequired,
                "Google authorization could not be refreshed",
            )
        })?;
        let read_only = drive_from(&authorization)?;
        let observed = read_only.account_identity().map_err(map_drive_error)?;
        exact_account(Some(&account_id), &observed.permission_id)?;
        let mut refresh_attempted = false;

        let mut detached = detach_store(&app, Arc::clone(&runtime), session_id, &vault)?;
        let run_result = (|| {
            let store = detached.store_mut()?;
            store
                .resume_auth_required_transfers(now_unix_ms()?)
                .map_err(map_sync_error)?;
            let mut metadata_fresh = false;
            for page_index in 0..MAX_INCREMENTAL_PAGES {
                let active = store.active_change_batch().map_err(map_sync_error)?;
                let (batch_id, final_page) = if let Some(active) = active {
                    requeue_active_batch_reconciliation(store, active.batch_id, now_unix_ms()?)?;
                    (active.batch_id, false)
                } else {
                    let read_only = drive_from(&authorization)?;
                    let transfer =
                        transfer_drive_from(&authorization, &account_id, &bound.remote_root_id)?;
                    let batch = prepare_incremental_change_batch(
                        &app,
                        &vault,
                        store,
                        &read_only,
                        &transfer,
                        &bound.remote_root_id,
                    )?;
                    (batch.batch_id, batch.final_page)
                };
                let _ = run_guarded_worker_with_auth_refresh(
                    &app,
                    &vault,
                    store,
                    &account_id,
                    &bound.remote_root_id,
                    &mut authorization,
                    &mut refresh_attempted,
                )?;
                let active = store
                    .active_change_batch()
                    .map_err(map_sync_error)?
                    .ok_or_else(SyncCommandError::internal)?;
                if active.applying_mutations == 0
                    && active.committed_mutations == active.declared_mutations
                {
                    store
                        .commit_transfer_change_batch(batch_id, now_unix_ms()?)
                        .map_err(map_sync_error)?;
                    if final_page {
                        metadata_fresh = true;
                        break;
                    }
                } else {
                    break;
                }
                if page_index + 1 == MAX_INCREMENTAL_PAGES {
                    return Err(SyncCommandError::new(
                        SyncCommandCode::RescanRequired,
                        "the bounded Drive changes drain did not finish",
                    ));
                }
            }
            if metadata_fresh {
                let transfer =
                    transfer_drive_from(&authorization, &account_id, &bound.remote_root_id)?;
                prepare_guarded_transfers(&app, &vault, store, &transfer, &bound.remote_root_id)?;
                let _ = run_guarded_worker_with_auth_refresh(
                    &app,
                    &vault,
                    store,
                    &account_id,
                    &bound.remote_root_id,
                    &mut authorization,
                    &mut refresh_attempted,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = run_result {
            detached.discard_store()?;
            return Err(error);
        }
        let mut restored = false;
        let finalized = with_current_session(&app, session_id, &vault, || {
            detached.restore_inner()?;
            restored = true;
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            state.account_id = Some(account_id);
            state.authorization = Some(authorization);
            let (persisted, transfers) = {
                let store = ensure_store(&app, &mut state, session_id, &vault)?;
                (
                    store.vault_state().map_err(map_sync_error)?,
                    store.transfer_summary().map_err(map_sync_error)?,
                )
            };
            Ok(android_status(
                session_id,
                &state,
                persisted.as_ref(),
                transfers,
            ))
        });
        match finalized {
            Err(error) if !restored => {
                detached.discard_store()?;
                Err(error)
            }
            result => result,
        }
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_run_guarded(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let session_id = validate_session(&session, &session_id)?;
        let vault = android_vault_capability(&session, session_id).map_err(map_app_error)?;
        let runtime = Arc::clone(runtime.inner());
        let result = tauri::async_runtime::spawn_blocking(move || {
            sync_run_guarded_impl(app, runtime, session_id, vault)
        })
        .await
        .map_err(|_| SyncCommandError::internal())??;
        android_vault_capability(&session, session_id).map_err(map_app_error)?;
        Ok(result)
    }

    fn sync_disconnect_impl(
        app: tauri::AppHandle,
        runtime: Arc<SyncRuntime>,
        requested: VaultSessionId,
        vault: SafVaultCapability,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let authorization = {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if state.transfer_running_session.is_some() {
                return Err(SyncCommandError::new(
                    SyncCommandCode::Busy,
                    "a guarded transfer is already running",
                ));
            }
            state.transfer_running_session = Some(requested);
            state.authorization.take()
        };
        let disconnect_failed = authorization
            .as_ref()
            .is_some_and(|current| app.google_auth().disconnect(&current.access_token).is_err());
        if disconnect_failed {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if state.transfer_running_session == Some(requested) {
                state.transfer_running_session = None;
                state.authorization = authorization;
            }
            return Err(SyncCommandError::new(
                SyncCommandCode::ProviderUnavailable,
                "Google authorization cleanup did not complete",
            ));
        }
        let finalized = with_current_session(&app, requested, &vault, || {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if state.transfer_running_session != Some(requested) {
                return Err(SyncCommandError::internal());
            }
            state.transfer_running_session = None;
            state.account_id = None;
            state.root_name = None;
            let (persisted, transfers) = {
                let store = ensure_store(&app, &mut state, requested, &vault)?;
                (
                    store.vault_state().map_err(map_sync_error)?,
                    store.transfer_summary().map_err(map_sync_error)?,
                )
            };
            Ok(android_status(
                requested,
                &state,
                persisted.as_ref(),
                transfers,
            ))
        });
        if finalized.is_err() {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if state.transfer_running_session == Some(requested) {
                state.transfer_running_session = None;
            }
        }
        finalized
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_disconnect(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let requested = parse_session_id(&session_id)?;
        let vault = android_vault_capability(&session, requested).map_err(map_app_error)?;
        let runtime = Arc::clone(runtime.inner());
        let result = tauri::async_runtime::spawn_blocking(move || {
            sync_disconnect_impl(app, runtime, requested, vault)
        })
        .await
        .map_err(|_| SyncCommandError::internal())??;
        android_vault_capability(&session, requested).map_err(map_app_error)?;
        Ok(result)
    }
}

pub use platform::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_errors_are_camel_case_and_redacted() {
        let error = SyncCommandError::new(
            SyncCommandCode::ProviderUnavailable,
            "Google Drive metadata is temporarily unavailable",
        );
        let json = serde_json::to_string(&error).expect("serialize safe error");
        assert_eq!(
            json,
            r#"{"code":"providerUnavailable","message":"Google Drive metadata is temporarily unavailable"}"#
        );
        for forbidden in ["token", "authorization", "/Users/", "provider body"] {
            assert!(!json.to_lowercase().contains(&forbidden.to_lowercase()));
        }
    }

    #[test]
    fn status_and_preview_serialize_with_exact_session_identity_contract() {
        let session_id =
            VaultSessionId::parse("12345678-1234-4abc-8def-1234567890ab").expect("session id");
        let status = SyncStatusDto {
            session_id,
            supported: true,
            binding_available: true,
            configured: false,
            connected: false,
            bound: false,
            account_id: None,
            root_id: None,
            root_name: None,
            phase: "unbound",
            rescan_required: false,
            active: 7,
            pending: 2,
            retry_scheduled: 1,
            auth_required: 1,
            needs_reconcile: 1,
            completed: 3,
        };
        let status_json = serde_json::to_string(&status).expect("serialize status");
        assert_eq!(
            status_json,
            r#"{"sessionId":"12345678-1234-4abc-8def-1234567890ab","supported":true,"bindingAvailable":true,"configured":false,"connected":false,"bound":false,"accountId":null,"rootId":null,"rootName":null,"phase":"unbound","rescanRequired":false,"active":7,"pending":2,"retryScheduled":1,"authRequired":1,"needsReconcile":1,"completed":3}"#
        );
        for forbidden in ["operationId", "path", "token", "sessionUri", "providerBody"] {
            assert!(!status_json.contains(forbidden));
        }

        let page = preview_page(
            session_id,
            RemotePreviewPage {
                entries: vec![myvault_sync_engine::RemotePreviewEntry {
                    file_id: "file_1".to_owned(),
                    parent_id: "folder_1".to_owned(),
                    path: "Notes/one.md".to_owned(),
                    kind: RemoteEntryKind::File,
                    path_collision: false,
                }],
                next_after: None,
                has_more: false,
                total_entries: 1,
                colliding_entries: 0,
                rescan_required: false,
            },
        )
        .expect("preview DTO");
        let preview_json = serde_json::to_string(&page).expect("serialize preview");
        assert_eq!(
            preview_json,
            r#"{"sessionId":"12345678-1234-4abc-8def-1234567890ab","entries":[{"fileId":"file_1","path":"Notes/one.md","kind":"file","pathCollision":false}],"nextAfter":null,"hasMore":false}"#
        );
    }

    #[test]
    fn preview_cursor_round_trip_is_opaque_and_bounded() {
        let cursor = RemotePreviewCursor {
            path: "Notes/ภาษาไทย.md".to_owned(),
            file_id: "file_123".to_owned(),
        };
        let encoded = encode_preview_cursor(&cursor).expect("encode cursor");
        assert!(!encoded.contains("Notes"));
        assert_eq!(
            decode_preview_cursor(Some(&encoded)).expect("decode cursor"),
            Some(cursor)
        );
        assert!(decode_preview_cursor(Some("not+base64")).is_err());
        assert!(decode_preview_cursor(Some(&"x".repeat(MAX_CURSOR_BYTES * 2 + 1))).is_err());
    }

    #[test]
    fn invalid_or_path_shaped_session_is_rejected_without_echo() {
        let error = parse_session_id("/Users/private/vault").expect_err("invalid session");
        assert_eq!(error.code, SyncCommandCode::InvalidRequest);
        let json = serde_json::to_string(&error).expect("serialize safe error");
        assert!(!json.contains("/Users/private/vault"));
    }

    #[test]
    fn folder_page_exposes_only_ids_names_and_pagination() {
        let session_id =
            VaultSessionId::parse("12345678-1234-4abc-8def-1234567890ab").expect("session id");
        let page = FilePage {
            files: vec![myvault_drive::RemoteFile {
                id: "folder_1".to_owned(),
                name: "Duplicate".to_owned(),
                mime_type: myvault_drive::FOLDER_MIME_TYPE.to_owned(),
                parents: vec!["root".to_owned()],
                trashed: false,
                version: Some("1".to_owned()),
                md5_checksum: None,
                sha1_checksum: None,
                sha256_checksum: None,
            }],
            next_page_token: Some("next_1".to_owned()),
            incomplete_search: false,
        };
        let json = serde_json::to_string(&folder_page(session_id, Some("root".to_owned()), page))
            .expect("serialize folders");
        assert_eq!(
            json,
            r#"{"sessionId":"12345678-1234-4abc-8def-1234567890ab","parentId":"root","folders":[{"id":"folder_1","name":"Duplicate"}],"nextPageToken":"next_1"}"#
        );
        assert!(!json.contains("mimeType"));
        assert!(!json.contains("parents"));
    }
}
