use serde::Serialize;
#[cfg(target_os = "android")]
use std::sync::Mutex;

#[cfg(target_os = "android")]
use tauri_plugin_google_auth::GoogleAuthExt;

#[cfg(target_os = "android")]
const GOOGLE_DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlatformInfo {
    os: &'static str,
    arch: &'static str,
    family: &'static str,
    debug_build: bool,
}

fn platform_info() -> PlatformInfo {
    PlatformInfo {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        family: std::env::consts::FAMILY,
        debug_build: cfg!(debug_assertions),
    }
}

#[tauri::command]
fn get_platform_info() -> PlatformInfo {
    platform_info()
}

#[derive(Default)]
struct GoogleAuthSession {
    #[cfg(target_os = "android")]
    authorization: Mutex<Option<tauri_plugin_google_auth::Authorization>>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct GoogleAuthStatus {
    supported: bool,
    connected: bool,
    granted_scope_count: usize,
}

#[cfg(not(target_os = "android"))]
impl GoogleAuthStatus {
    fn unsupported() -> Self {
        Self {
            supported: false,
            connected: false,
            granted_scope_count: 0,
        }
    }
}

#[tauri::command]
fn google_auth_status(
    session: tauri::State<'_, GoogleAuthSession>,
) -> Result<GoogleAuthStatus, String> {
    #[cfg(target_os = "android")]
    {
        let authorization = session
            .authorization
            .lock()
            .map_err(|_| "Authorization state is unavailable".to_owned())?;
        return Ok(GoogleAuthStatus {
            supported: true,
            connected: authorization.is_some(),
            granted_scope_count: authorization
                .as_ref()
                .map_or(0, |value| value.granted_scope_count),
        });
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = session;
        Ok(GoogleAuthStatus::unsupported())
    }
}

#[tauri::command]
fn google_auth_connect(
    app: tauri::AppHandle,
    session: tauri::State<'_, GoogleAuthSession>,
) -> Result<GoogleAuthStatus, String> {
    #[cfg(target_os = "android")]
    {
        let authorization = app
            .google_auth()
            .authorize(&[GOOGLE_DRIVE_SCOPE])
            .map_err(|_| "Google authorization failed".to_owned())?;
        let scope_count = authorization.granted_scope_count;
        *session
            .authorization
            .lock()
            .map_err(|_| "Authorization state is unavailable".to_owned())? = Some(authorization);
        return Ok(GoogleAuthStatus {
            supported: true,
            connected: true,
            granted_scope_count: scope_count,
        });
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, session);
        Ok(GoogleAuthStatus::unsupported())
    }
}

#[tauri::command]
fn google_auth_disconnect(
    app: tauri::AppHandle,
    session: tauri::State<'_, GoogleAuthSession>,
) -> Result<GoogleAuthStatus, String> {
    #[cfg(target_os = "android")]
    {
        let mut authorization = session
            .authorization
            .lock()
            .map_err(|_| "Authorization state is unavailable".to_owned())?;

        if let Some(current) = authorization.as_ref() {
            app.google_auth()
                .disconnect(&current.access_token)
                .map_err(|_| "Google disconnect failed".to_owned())?;
        }
        authorization.take();

        return Ok(GoogleAuthStatus {
            supported: true,
            connected: false,
            granted_scope_count: 0,
        });
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, session);
        Ok(GoogleAuthStatus::unsupported())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().manage(GoogleAuthSession::default());

    #[cfg(target_os = "android")]
    let builder = builder
        .plugin(tauri_plugin_google_auth::init())
        .plugin(tauri_plugin_private_root::init());

    builder
        .invoke_handler(tauri::generate_handler![
            get_platform_info,
            google_auth_status,
            google_auth_connect,
            google_auth_disconnect
        ])
        .run(tauri::generate_context!())
        .expect("error while running myVault");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_probe_reports_non_empty_constants() {
        let info = platform_info();

        assert!(!info.os.is_empty());
        assert!(!info.arch.is_empty());
        assert!(!info.family.is_empty());
    }

    #[test]
    fn unsupported_auth_status_is_frontend_safe() {
        let status = GoogleAuthStatus::unsupported();

        assert!(!status.supported);
        assert!(!status.connected);
        assert_eq!(status.granted_scope_count, 0);
        assert!(!serde_json::to_string(&status)
            .expect("status should serialize")
            .contains("token"));
    }
}
