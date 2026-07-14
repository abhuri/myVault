#![cfg_attr(target_os = "android", allow(dead_code))]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use myvault_app_service::{AppError, AppErrorCode, VaultSessionId};
use myvault_drive::{ErrorCode as DriveErrorCode, FilePage};
use myvault_sync_engine::{
    InitialSyncProgress, RemoteEntryKind, RemotePreviewCursor, RemotePreviewPage, SyncPhase,
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
    use myvault_app_service::{AppService, NativeVaultContext};
    use myvault_desktop_auth::{
        DesktopOAuth, GoogleTokenClient, NativeTokenProvider, OsKeyringStore, SecretStore,
    };
    use myvault_drive::{AccessToken, ReadOnlyDrive};
    use myvault_sync_engine::{advance_initial_sync, BindOutcome, SyncStore, VaultSyncState};
    use std::{
        process::{Command, Stdio},
        sync::{Arc, Mutex},
        time::Duration,
    };

    const CLIENT_ID_ENV: &str = "MYVAULT_GOOGLE_DESKTOP_CLIENT_ID";
    const KEYRING_SERVICE: &str = "com.abhuri.myvault.google-drive";
    const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);

    #[derive(Default)]
    pub struct SyncRuntime {
        inner: Mutex<RuntimeInner>,
    }

    #[derive(Default)]
    struct RuntimeInner {
        connected_account_id: Option<String>,
        root_name: Option<String>,
        active: Option<ActiveSync>,
    }

    struct ActiveSync {
        session_id: VaultSessionId,
        store: SyncStore,
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
        desktop_client_id().is_ok()
    }

    fn provider(
        client_id: &str,
    ) -> Result<NativeTokenProvider<GoogleTokenClient, OsKeyringStore>, SyncCommandError> {
        let endpoint = GoogleTokenClient::new().map_err(|_| SyncCommandError::internal())?;
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

    fn fresh_drive(account_id: &str) -> Result<ReadOnlyDrive, SyncCommandError> {
        let client_id = desktop_client_id()?;
        let access = provider(&client_id)?
            .fresh_access_token(account_id)
            .map_err(|_| {
                SyncCommandError::new(
                    SyncCommandCode::AuthRequired,
                    "Google authorization is required",
                )
            })?;
        ReadOnlyDrive::google(AccessToken::new(access.expose_to_native().to_owned()))
            .map_err(map_drive_error)
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
                let state = ensure_store(&mut inner, &context)?
                    .vault_state()
                    .map_err(map_sync_error)?;
                refresh_connected_state(&mut inner, state.as_ref())?;
                Ok(status_from(session_id, &inner, state.as_ref()))
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
                    let provider = provider(&client_id)?;
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
                    let state = ensure_store(&mut inner, &context)?
                        .vault_state()
                        .map_err(map_sync_error)?;
                    Ok(status_from(session_id, &inner, state.as_ref()))
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
                    let (outcome, state) = {
                        let store = ensure_store(&mut inner, &context)?;
                        let outcome = store
                            .bind_remote_root(&binding, now_unix_ms()?)
                            .map_err(map_sync_error)?;
                        let state = store.vault_state().map_err(map_sync_error)?;
                        (outcome, state)
                    };
                    inner.root_name = Some(root.name);
                    Ok(BindRootDto {
                        session_id,
                        outcome: match outcome {
                            BindOutcome::Created => "created",
                            BindOutcome::AlreadyBound => "alreadyBound",
                            BindOutcome::LegacyBindingConfirmed => "legacyBindingConfirmed",
                        },
                        status: status_from(session_id, &inner, state.as_ref()),
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
                    Ok(ScanStepDto {
                        session_id,
                        progress: progress_name(progress),
                        status: status_from(session_id, &inner, state.as_ref()),
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
                    let state = ensure_store(&mut inner, &context)?
                        .vault_state()
                        .map_err(map_sync_error)?;
                    Ok(status_from(session_id, &inner, state.as_ref()))
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
    }
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use crate::app_commands::{
        android_session_id, with_android_session_lease, AndroidVaultSession,
    };
    use myvault_drive::{AccessToken, ReadOnlyDrive};
    use std::sync::{Arc, Mutex};
    use tauri_plugin_google_auth::{Authorization, GoogleAuthExt};

    #[derive(Default)]
    pub struct SyncRuntime {
        inner: Mutex<AndroidSyncState>,
    }

    #[derive(Default)]
    struct AndroidSyncState {
        account_id: Option<String>,
        root_name: Option<String>,
        authorization: Option<Authorization>,
    }

    fn validate_session(
        session: &AndroidVaultSession,
        session_id: &str,
    ) -> Result<VaultSessionId, SyncCommandError> {
        let requested = parse_session_id(session_id)?;
        android_session_id(session, requested).map_err(map_app_error)
    }

    fn android_status(session_id: VaultSessionId, state: &AndroidSyncState) -> SyncStatusDto {
        SyncStatusDto {
            session_id,
            supported: true,
            binding_available: false,
            configured: true,
            connected: state.authorization.is_some(),
            bound: false,
            account_id: state.account_id.clone(),
            root_id: None,
            root_name: state.root_name.clone(),
            phase: "unbound",
            rescan_required: false,
        }
    }

    fn drive_from(authorization: &Authorization) -> Result<ReadOnlyDrive, SyncCommandError> {
        authorization
            .with_native_access_token(|token| {
                ReadOnlyDrive::google(AccessToken::new(token.to_owned()))
            })
            .map_err(map_drive_error)
    }

    #[tauri::command(rename_all = "camelCase")]
    pub fn sync_status(
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let session_id = validate_session(&session, &session_id)?;
        let state = runtime
            .inner
            .lock()
            .map_err(|_| SyncCommandError::internal())?;
        Ok(android_status(session_id, &state))
    }

    #[tauri::command(rename_all = "camelCase")]
    pub fn sync_connect(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let requested = parse_session_id(&session_id)?;
        with_android_session_lease(&session, requested, || {
            let authorization = app.google_auth().fresh_access_token().map_err(|_| {
                SyncCommandError::new(
                    SyncCommandCode::AuthRequired,
                    "Google authorization could not be completed",
                )
            })?;
            let drive = drive_from(&authorization)?;
            let account = drive.account_identity().map_err(map_drive_error)?;
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            state.account_id = Some(account.permission_id);
            state.authorization = Some(authorization);
            Ok(android_status(requested, &state))
        })
        .map_err(map_app_error)?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub fn sync_list_folders(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
        parent_id: Option<String>,
        page_token: Option<String>,
    ) -> Result<RemoteFolderPageDto, SyncCommandError> {
        let requested = parse_session_id(&session_id)?;
        with_android_session_lease(&session, requested, || {
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
            if expected_account_id != account.permission_id {
                return Err(SyncCommandError::new(
                    SyncCommandCode::BindingMismatch,
                    "the Google account does not match the connected account",
                ));
            }
            let page = drive
                .list_children_page(
                    parent_id.as_deref().unwrap_or("root"),
                    page_token.as_deref(),
                )
                .map_err(map_drive_error)?;
            runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?
                .authorization = Some(authorization);
            Ok(folder_page(requested, parent_id, page))
        })
        .map_err(map_app_error)?
    }

    #[tauri::command(rename_all = "camelCase")]
    pub fn sync_bind_root(
        session: tauri::State<'_, AndroidVaultSession>,
        session_id: String,
        account_id: String,
        root_id: String,
    ) -> Result<BindRootDto, SyncCommandError> {
        validate_session(&session, &session_id)?;
        let _ = (account_id, root_id);
        Err(SyncCommandError::unsupported())
    }

    #[tauri::command(rename_all = "camelCase")]
    pub fn sync_scan_step(
        session: tauri::State<'_, AndroidVaultSession>,
        session_id: String,
    ) -> Result<ScanStepDto, SyncCommandError> {
        validate_session(&session, &session_id)?;
        Err(SyncCommandError::unsupported())
    }

    #[tauri::command(rename_all = "camelCase")]
    pub fn sync_preview(
        session: tauri::State<'_, AndroidVaultSession>,
        session_id: String,
        after: Option<String>,
        limit: Option<usize>,
    ) -> Result<RemotePreviewPageDto, SyncCommandError> {
        validate_session(&session, &session_id)?;
        let _ = (after, limit);
        Err(SyncCommandError::unsupported())
    }

    #[tauri::command(rename_all = "camelCase")]
    pub fn sync_disconnect(
        app: tauri::AppHandle,
        session: tauri::State<'_, AndroidVaultSession>,
        runtime: tauri::State<'_, Arc<SyncRuntime>>,
        session_id: String,
    ) -> Result<SyncStatusDto, SyncCommandError> {
        let requested = parse_session_id(&session_id)?;
        with_android_session_lease(&session, requested, || {
            let mut state = runtime
                .inner
                .lock()
                .map_err(|_| SyncCommandError::internal())?;
            if let Some(authorization) = state.authorization.as_ref() {
                app.google_auth()
                    .disconnect(&authorization.access_token)
                    .map_err(|_| {
                        SyncCommandError::new(
                            SyncCommandCode::ProviderUnavailable,
                            "Google authorization cleanup did not complete",
                        )
                    })?;
            }
            state.account_id = None;
            state.root_name = None;
            state.authorization = None;
            Ok(android_status(requested, &state))
        })
        .map_err(map_app_error)?
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
        };
        let status_json = serde_json::to_string(&status).expect("serialize status");
        assert_eq!(
            status_json,
            r#"{"sessionId":"12345678-1234-4abc-8def-1234567890ab","supported":true,"bindingAvailable":true,"configured":false,"connected":false,"bound":false,"accountId":null,"rootId":null,"rootName":null,"phase":"unbound","rescanRequired":false}"#
        );

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
