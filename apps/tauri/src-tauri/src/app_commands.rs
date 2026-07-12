use myvault_app_service::{
    AppError, AppService, NoteDto, TrashPageDto, VaultSessionId, VaultStatusDto,
};
use std::sync::Arc;

const DEFAULT_TRASH_LIMIT: usize = 50;

fn parse_session_id(value: &str) -> Result<VaultSessionId, AppError> {
    VaultSessionId::parse(value)
}

fn resolved_trash_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_TRASH_LIMIT)
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
}
