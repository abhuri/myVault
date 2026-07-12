use myvault_app_service::{
    AppError, AppService, ExplorerPageDto, NoteDto, SaveNoteDto, TrashPageDto, VaultSessionId,
    VaultStatusDto, EXPLORER_DEFAULT_PAGE_SIZE,
};
use serde::Serialize;
use std::sync::Arc;

#[cfg(not(target_os = "android"))]
use myvault_core::Vault;
#[cfg(not(target_os = "android"))]
use std::path::PathBuf;
#[cfg(not(target_os = "android"))]
use tauri_plugin_dialog::{DialogExt, FilePath};

const DEFAULT_TRASH_LIMIT: usize = 50;

fn parse_session_id(value: &str) -> Result<VaultSessionId, AppError> {
    VaultSessionId::parse(value)
}

fn resolved_trash_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_TRASH_LIMIT)
}

fn resolved_explorer_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(EXPLORER_DEFAULT_PAGE_SIZE)
}

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

#[tauri::command]
pub fn vault_status(
    service: tauri::State<'_, Arc<AppService>>,
) -> Result<VaultStatusDto, AppError> {
    service.status()
}

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

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn vault_choose_folder(
    app: tauri::AppHandle,
    service: tauri::State<'_, Arc<AppService>>,
) -> Result<VaultChooseFolderDto, AppError> {
    let service = Arc::clone(service.inner());
    tauri::async_runtime::spawn_blocking(move || {
        let selection = match app.dialog().file().blocking_pick_folder() {
            None => PickerSelection::Cancelled,
            Some(FilePath::Path(path)) => PickerSelection::Path(path),
            Some(FilePath::Url(_)) => PickerSelection::Url,
        };
        apply_picker_selection(&service, selection)
    })
    .await
    .map_err(|_| AppError::internal())?
}

#[cfg(target_os = "android")]
#[tauri::command]
pub async fn vault_choose_folder() -> Result<VaultChooseFolderDto, AppError> {
    Err(AppError::unsupported_platform())
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
}
