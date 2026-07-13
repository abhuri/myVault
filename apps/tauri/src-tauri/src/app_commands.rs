use myvault_app_service::{
    AppError, ExplorerPageDto, NoteDto, SaveNoteDto, TrashPageDto, VaultSessionId, VaultStatusDto,
    EXPLORER_DEFAULT_PAGE_SIZE,
};
use serde::Serialize;

#[cfg(not(target_os = "android"))]
use myvault_app_service::AppService;
#[cfg(not(target_os = "android"))]
use std::sync::Arc;

#[cfg(target_os = "android")]
use std::sync::Mutex;
#[cfg(target_os = "android")]
use tauri_plugin_vault_saf::{SafError, VaultSafExt};

#[cfg(not(target_os = "android"))]
use myvault_core::Vault;
#[cfg(not(target_os = "android"))]
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
#[cfg(not(target_os = "android"))]
use std::path::PathBuf;
#[cfg(not(target_os = "android"))]
use std::sync::Mutex;
#[cfg(not(target_os = "android"))]
use tauri::Emitter;
#[cfg(not(target_os = "android"))]
use tauri_plugin_dialog::{DialogExt, FilePath};

const DEFAULT_TRASH_LIMIT: usize = 50;

#[cfg(not(target_os = "android"))]
#[derive(Clone, Default)]
pub struct DesktopVaultWatcher(Arc<Mutex<Option<RecommendedWatcher>>>);

#[cfg(not(target_os = "android"))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct VaultChangedEvent {
    session_id: VaultSessionId,
}

#[cfg(not(target_os = "android"))]
impl DesktopVaultWatcher {
    fn replace(
        &self,
        app: tauri::AppHandle,
        root: &std::path::Path,
        session_id: VaultSessionId,
    ) -> Result<(), AppError> {
        self.replace_with_handler(root, move |event| {
            if event.is_ok() {
                let _ = app.emit("myvault-vault-changed", VaultChangedEvent { session_id });
            }
        })
    }

    fn replace_with_handler(
        &self,
        root: &std::path::Path,
        handler: impl FnMut(notify::Result<notify::Event>) + Send + 'static,
    ) -> Result<(), AppError> {
        let mut watcher =
            notify::recommended_watcher(handler).map_err(|_| AppError::vault_unavailable())?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|_| AppError::vault_unavailable())?;
        *self.0.lock().map_err(|_| AppError::internal())? = Some(watcher);
        Ok(())
    }
}

#[cfg(target_os = "android")]
#[derive(Default)]
pub struct AndroidVaultSession(Mutex<Option<VaultSessionId>>);

#[cfg(target_os = "android")]
fn android_session_id(
    session: &AndroidVaultSession,
    requested: VaultSessionId,
) -> Result<VaultSessionId, AppError> {
    let active = *session.0.lock().map_err(|_| AppError::internal())?;
    validate_android_session(active, requested)
}

#[cfg(any(target_os = "android", test))]
fn validate_android_session(
    active: Option<VaultSessionId>,
    requested: VaultSessionId,
) -> Result<VaultSessionId, AppError> {
    match active {
        None => Err(AppError::no_active_session()),
        Some(value) if value == requested => Ok(value),
        Some(_) => Err(AppError::stale_session()),
    }
}

#[cfg(target_os = "android")]
fn map_android_saf_error(error: SafError) -> AppError {
    use myvault_app_service::AppErrorCode;

    match error {
        SafError::InvalidPath => AppError {
            code: AppErrorCode::InvalidPath,
            message: "the note path is invalid",
        },
        SafError::NoteNotFound => AppError::note_not_found(),
        SafError::NoteNotUtf8 => AppError {
            code: AppErrorCode::NoteNotUtf8,
            message: "the note is not valid UTF-8",
        },
        SafError::ResourceLimit => AppError {
            code: AppErrorCode::ResourceLimit,
            message: "the requested vault evidence exceeds a safety limit",
        },
        SafError::VaultUnavailable | SafError::NativeBridge => AppError::vault_unavailable(),
        SafError::PickerBusy
        | SafError::PickerUnavailable
        | SafError::PickerPermission
        | SafError::PickerFailed => AppError::vault_selection_failed(),
    }
}

fn parse_session_id(value: &str) -> Result<VaultSessionId, AppError> {
    VaultSessionId::parse(value)
}

fn resolved_trash_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_TRASH_LIMIT)
}

fn resolved_explorer_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(EXPLORER_DEFAULT_PAGE_SIZE)
}

#[cfg_attr(target_os = "android", allow(dead_code))]
fn map_save_join_failure() -> AppError {
    AppError::write_outcome_unknown()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "outcome",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[cfg_attr(target_os = "android", allow(dead_code))]
pub enum VaultChooseFolderDto {
    Activated { status: VaultStatusDto },
    Cancelled,
}

#[cfg(not(target_os = "android"))]
enum PickerSelection {
    Cancelled,
    Path(PathBuf),
    Url,
}

#[cfg(not(target_os = "android"))]
#[cfg(test)]
fn apply_picker_selection(
    service: &AppService,
    selection: PickerSelection,
) -> Result<VaultChooseFolderDto, AppError> {
    match selection {
        PickerSelection::Cancelled => Ok(VaultChooseFolderDto::Cancelled),
        PickerSelection::Path(path) => {
            let vault = Vault::open(path).map_err(|_| AppError::vault_selection_failed())?;
            let status = service.activate_trusted_vault(vault)?;
            Ok(VaultChooseFolderDto::Activated { status })
        }
        PickerSelection::Url => Err(AppError::vault_selection_failed()),
    }
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub fn vault_status(
    service: tauri::State<'_, Arc<AppService>>,
) -> Result<VaultStatusDto, AppError> {
    service.status()
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn vault_status(
    app: tauri::AppHandle,
    session: tauri::State<'_, AndroidVaultSession>,
) -> Result<VaultStatusDto, AppError> {
    let native_active = app.vault_saf().has_root().map_err(map_android_saf_error)?;
    let mut current = session.0.lock().map_err(|_| AppError::internal())?;
    if native_active && current.is_none() {
        *current = Some(VaultSessionId::new());
    } else if !native_active {
        *current = None;
    }
    Ok(VaultStatusDto {
        active: native_active,
        session_id: *current,
    })
}

#[cfg(not(target_os = "android"))]
#[tauri::command(rename_all = "camelCase")]
pub async fn vault_read_note(
    service: tauri::State<'_, Arc<AppService>>,
    session_id: String,
    path: String,
) -> Result<NoteDto, AppError> {
    let session_id = parse_session_id(&session_id)?;
    let service = Arc::clone(service.inner());
    tauri::async_runtime::spawn_blocking(move || service.read_note(session_id, &path))
        .await
        .map_err(|_| AppError::internal())?
}

#[cfg(target_os = "android")]
#[tauri::command(rename_all = "camelCase")]
pub async fn vault_read_note(
    app: tauri::AppHandle,
    session: tauri::State<'_, AndroidVaultSession>,
    session_id: String,
    path: String,
) -> Result<NoteDto, AppError> {
    let requested = parse_session_id(&session_id)?;
    android_session_id(&session, requested)?;
    let note = app
        .vault_saf()
        .read_note(&path)
        .map_err(map_android_saf_error)?;
    android_session_id(&session, requested)?;
    Ok(NoteDto {
        session_id: requested,
        path,
        text: note.text,
        revision_hex: note.revision_hex,
        byte_len: note.byte_len,
    })
}

#[cfg(not(target_os = "android"))]
#[tauri::command(rename_all = "camelCase")]
pub async fn vault_save_note(
    service: tauri::State<'_, Arc<AppService>>,
    session_id: String,
    path: String,
    text: String,
    expected_revision_hex: String,
    expected_byte_len: u64,
) -> Result<SaveNoteDto, AppError> {
    let session_id = parse_session_id(&session_id)?;
    let service = Arc::clone(service.inner());
    tauri::async_runtime::spawn_blocking(move || {
        service.save_note(
            session_id,
            &path,
            &text,
            &expected_revision_hex,
            expected_byte_len,
        )
    })
    .await
    .map_err(|_| map_save_join_failure())?
}

#[cfg(target_os = "android")]
#[tauri::command(rename_all = "camelCase")]
pub async fn vault_save_note(
    app: tauri::AppHandle,
    session: tauri::State<'_, AndroidVaultSession>,
    session_id: String,
    path: String,
    text: String,
    expected_revision_hex: String,
    expected_byte_len: u64,
) -> Result<SaveNoteDto, AppError> {
    let requested = parse_session_id(&session_id)?;
    android_session_id(&session, requested)?;
    let saved = app
        .vault_saf()
        .save_note(&path, &text, &expected_revision_hex, expected_byte_len)
        .map_err(|error| match error {
            tauri_plugin_vault_saf::SafSaveError::StaleRevision => AppError::stale_revision(),
            tauri_plugin_vault_saf::SafSaveError::NoteNotFound => AppError::note_not_found(),
            tauri_plugin_vault_saf::SafSaveError::InvalidPath => AppError {
                code: myvault_app_service::AppErrorCode::InvalidPath,
                message: "the note path is invalid",
            },
            tauri_plugin_vault_saf::SafSaveError::InvalidRequest => {
                AppError::invalid_revision_or_path()
            }
            tauri_plugin_vault_saf::SafSaveError::WriteOutcomeUnknown
            | tauri_plugin_vault_saf::SafSaveError::NativeBridge => {
                AppError::write_outcome_unknown()
            }
        })?;
    android_session_id(&session, requested)?;
    Ok(SaveNoteDto {
        session_id: requested,
        path,
        revision_hex: saved.revision_hex,
        byte_len: saved.byte_len,
        durability: myvault_app_service::SaveDurabilityDto::DirectorySyncUnsupported,
    })
}

#[cfg(not(target_os = "android"))]
#[tauri::command(rename_all = "camelCase")]
pub async fn vault_list_trash(
    service: tauri::State<'_, Arc<AppService>>,
    session_id: String,
    after: Option<String>,
    limit: Option<usize>,
) -> Result<TrashPageDto, AppError> {
    let session_id = parse_session_id(&session_id)?;
    let limit = resolved_trash_limit(limit);
    let service = Arc::clone(service.inner());
    tauri::async_runtime::spawn_blocking(move || {
        service.list_trash(session_id, after.as_deref(), limit)
    })
    .await
    .map_err(|_| AppError::internal())?
}

#[cfg(target_os = "android")]
#[tauri::command(rename_all = "camelCase")]
pub async fn vault_list_trash(
    session: tauri::State<'_, AndroidVaultSession>,
    session_id: String,
    after: Option<String>,
    limit: Option<usize>,
) -> Result<TrashPageDto, AppError> {
    let requested = parse_session_id(&session_id)?;
    android_session_id(&session, requested)?;
    if after.is_some() || !(1..=200).contains(&resolved_trash_limit(limit)) {
        return Err(AppError::invalid_cursor_or_limit());
    }
    Ok(TrashPageDto {
        session_id: requested,
        entries: Vec::new(),
        invalid_name_count: 0,
        next_after: None,
        has_more: false,
        scanned_entries: 0,
    })
}

#[cfg(not(target_os = "android"))]
#[tauri::command(rename_all = "camelCase")]
pub async fn vault_list_explorer(
    service: tauri::State<'_, Arc<AppService>>,
    session_id: String,
    after: Option<String>,
    limit: Option<usize>,
) -> Result<ExplorerPageDto, AppError> {
    let session_id = parse_session_id(&session_id)?;
    let limit = resolved_explorer_limit(limit);
    let service = Arc::clone(service.inner());
    tauri::async_runtime::spawn_blocking(move || {
        service.list_explorer(session_id, after.as_deref(), limit)
    })
    .await
    .map_err(|_| AppError::internal())?
}

#[cfg(target_os = "android")]
#[tauri::command(rename_all = "camelCase")]
pub async fn vault_list_explorer(
    app: tauri::AppHandle,
    session: tauri::State<'_, AndroidVaultSession>,
    session_id: String,
    after: Option<String>,
    limit: Option<usize>,
) -> Result<ExplorerPageDto, AppError> {
    let requested = parse_session_id(&session_id)?;
    android_session_id(&session, requested)?;
    let limit = resolved_explorer_limit(limit);
    if !(1..=myvault_app_service::EXPLORER_MAX_PAGE_SIZE).contains(&limit) {
        return Err(AppError::invalid_cursor_or_limit());
    }
    if after
        .as_deref()
        .is_some_and(|cursor| !tauri_plugin_vault_saf::is_valid_explorer_cursor(cursor))
    {
        return Err(AppError {
            code: myvault_app_service::AppErrorCode::InvalidCursor,
            message: "the explorer cursor is invalid",
        });
    }
    let mut inventory = app.vault_saf().inventory().map_err(map_android_saf_error)?;
    android_session_id(&session, requested)?;
    inventory.normalize_portable_order();
    let start = after.as_ref().map_or(0, |cursor| {
        inventory
            .entries
            .partition_point(|entry| entry.path <= *cursor)
    });
    let end = inventory.entries.len().min(start.saturating_add(limit));
    let entries = inventory.entries[start..end]
        .iter()
        .map(|entry| myvault_app_service::ExplorerEntryDto {
            path: entry.path.clone(),
            kind: if entry.kind == "markdown" {
                myvault_app_service::ExplorerKindDto::Markdown
            } else {
                myvault_app_service::ExplorerKindDto::File
            },
            byte_len: entry.byte_len,
        })
        .collect::<Vec<_>>();
    Ok(ExplorerPageDto {
        session_id: requested,
        next_after: entries.last().map(|entry| entry.path.clone()),
        has_more: end < inventory.entries.len(),
        scanned_entries: inventory.scanned_entries,
        entries,
    })
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn vault_choose_folder(
    app: tauri::AppHandle,
    service: tauri::State<'_, Arc<AppService>>,
    watcher: tauri::State<'_, DesktopVaultWatcher>,
) -> Result<VaultChooseFolderDto, AppError> {
    let service = Arc::clone(service.inner());
    let watcher = watcher.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let selection = match app.dialog().file().blocking_pick_folder() {
            None => PickerSelection::Cancelled,
            Some(FilePath::Path(path)) => PickerSelection::Path(path),
            Some(FilePath::Url(_)) => PickerSelection::Url,
        };
        match selection {
            PickerSelection::Cancelled => Ok(VaultChooseFolderDto::Cancelled),
            PickerSelection::Url => Err(AppError::vault_selection_failed()),
            PickerSelection::Path(path) => {
                let vault = Vault::open(path).map_err(|_| AppError::vault_selection_failed())?;
                let root = vault.root().to_path_buf();
                let status = service.activate_trusted_vault(vault)?;
                let session_id = status.session_id.ok_or_else(AppError::internal)?;
                if let Err(error) = watcher.replace(app, &root, session_id) {
                    let _ = service.close(session_id);
                    return Err(error);
                }
                Ok(VaultChooseFolderDto::Activated { status })
            }
        }
    })
    .await
    .map_err(|_| AppError::internal())?
}

#[cfg(target_os = "android")]
#[tauri::command]
pub async fn vault_choose_folder(
    app: tauri::AppHandle,
    session: tauri::State<'_, AndroidVaultSession>,
) -> Result<VaultChooseFolderDto, AppError> {
    let activated = app
        .vault_saf()
        .choose_root()
        .map_err(|_| AppError::vault_selection_failed())?;
    if !activated {
        return Ok(VaultChooseFolderDto::Cancelled);
    }
    let id = VaultSessionId::new();
    *session.0.lock().map_err(|_| AppError::internal())? = Some(id);
    Ok(VaultChooseFolderDto::Activated {
        status: VaultStatusDto {
            active: true,
            session_id: Some(id),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use myvault_app_service::AppErrorCode;

    #[test]
    fn command_inputs_require_canonical_session_and_default_trash_limit() {
        let canonical = "12345678-1234-4abc-8def-1234567890ab";
        assert_eq!(
            parse_session_id(canonical)
                .expect("canonical session")
                .to_string(),
            canonical
        );
        for invalid in [
            "12345678-1234-4ABC-8DEF-1234567890AB",
            "1234567812344abc8def1234567890ab",
            "00000000-0000-0000-0000-000000000000",
            "/private/ambient/vault",
        ] {
            assert_eq!(
                parse_session_id(invalid).expect_err("invalid session").code,
                AppErrorCode::InvalidSessionId
            );
        }
        assert_eq!(resolved_trash_limit(None), 50);
        assert_eq!(resolved_trash_limit(Some(1)), 1);
        assert_eq!(resolved_explorer_limit(None), 100);
        assert_eq!(resolved_explorer_limit(Some(1)), 1);
    }

    #[test]
    fn join_failure_error_is_frontend_safe_camel_case_json() {
        let json = serde_json::to_string(&AppError::internal()).expect("safe error JSON");
        assert_eq!(
            json,
            "{\"code\":\"internal\",\"message\":\"the application service is unavailable\"}"
        );
        assert!(!json.contains("path"));
        assert!(!json.contains("backtrace"));
    }

    #[test]
    fn save_join_failure_is_exact_write_outcome_unknown_json() {
        let json = serde_json::to_string(&map_save_join_failure()).expect("safe error JSON");
        assert_eq!(
            json,
            "{\"code\":\"writeOutcomeUnknown\",\"message\":\"the note write outcome is unknown\"}"
        );
        assert!(!json.contains("path"));
        assert!(!json.contains("backtrace"));
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn picker_adapter_preserves_old_session_until_valid_path_activation() {
        let temporary = tempfile::tempdir().expect("temporary");
        let old_root = temporary.path().join("old-root-secret");
        let new_root = temporary.path().join("new-root-secret");
        std::fs::create_dir(&old_root).expect("old root");
        std::fs::create_dir(&new_root).expect("new root");
        let old_root = old_root.canonicalize().expect("canonical old");
        let new_root = new_root.canonicalize().expect("canonical new");
        let service = AppService::new();
        let old_status = service
            .activate_trusted_vault(Vault::open(&old_root).expect("old vault"))
            .expect("activate old");

        assert_eq!(
            apply_picker_selection(&service, PickerSelection::Cancelled).expect("cancel"),
            VaultChooseFolderDto::Cancelled
        );
        assert_eq!(service.status().expect("status after cancel"), old_status);
        assert_eq!(
            apply_picker_selection(&service, PickerSelection::Url)
                .expect_err("URL rejected")
                .code,
            AppErrorCode::VaultSelectionFailed
        );
        assert_eq!(service.status().expect("status after URL"), old_status);
        assert_eq!(
            apply_picker_selection(
                &service,
                PickerSelection::Path(temporary.path().join("missing-root-secret")),
            )
            .expect_err("invalid path")
            .code,
            AppErrorCode::VaultSelectionFailed
        );
        assert_eq!(service.status().expect("status after invalid"), old_status);

        let activated = apply_picker_selection(&service, PickerSelection::Path(new_root))
            .expect("valid selection");
        let VaultChooseFolderDto::Activated { status } = &activated else {
            panic!("expected activation");
        };
        assert_ne!(status.session_id, old_status.session_id);
        let json = serde_json::to_string(&activated).expect("activation JSON");
        assert!(json.contains("\"outcome\":\"activated\""));
        assert!(json.contains("\"sessionId\""));
        assert!(!json.contains("old-root-secret"));
        assert!(!json.contains("new-root-secret"));
        assert!(!json.contains("missing-root-secret"));
        assert_eq!(
            serde_json::to_string(&VaultChooseFolderDto::Cancelled).expect("cancel JSON"),
            "{\"outcome\":\"cancelled\"}"
        );
    }

    #[test]
    fn android_session_policy_distinguishes_missing_matching_and_stale_ids() {
        let active = VaultSessionId::new();
        let stale = VaultSessionId::new();

        assert_eq!(
            validate_android_session(None, active)
                .expect_err("missing session")
                .code,
            myvault_app_service::AppErrorCode::NoActiveSession
        );
        assert_eq!(
            validate_android_session(Some(active), active).expect("matching session"),
            active
        );
        assert_eq!(
            validate_android_session(Some(active), stale)
                .expect_err("stale session")
                .code,
            myvault_app_service::AppErrorCode::StaleSession
        );
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn desktop_watcher_observes_external_file_change() {
        use std::{fs, sync::mpsc, time::Duration};

        let root = tempfile::tempdir().expect("watch root");
        let watcher = DesktopVaultWatcher::default();
        let (sender, receiver) = mpsc::channel();
        watcher
            .replace_with_handler(root.path(), move |event| {
                let _ = sender.send(event);
            })
            .expect("start watcher");
        fs::write(root.path().join("external.md"), "changed").expect("external write");
        let event = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("watch event")
            .expect("valid watch event");
        assert!(event.paths.iter().any(|path| path.ends_with("external.md")));
    }
}
