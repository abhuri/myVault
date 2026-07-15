use serde::Serialize;
use std::sync::Arc;

#[allow(dead_code)]
mod android_transfer_policy;
#[allow(dead_code)]
mod android_transfer_runtime;
mod app_commands;
mod sync_commands;
#[cfg(not(target_os = "android"))]
mod transfer_runtime;

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().manage(Arc::new(sync_commands::SyncRuntime::default()));

    #[cfg(target_os = "android")]
    let builder = builder
        .manage(app_commands::AndroidVaultSession::default())
        .plugin(tauri_plugin_google_auth::init())
        .plugin(tauri_plugin_private_root::init())
        .plugin(tauri_plugin_vault_saf::init());

    #[cfg(not(target_os = "android"))]
    let builder = builder.plugin(tauri_plugin_dialog::init()).setup(|app| {
        use tauri::Manager;

        let app_data = app.path().app_data_dir()?;
        std::fs::create_dir_all(&app_data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&app_data, std::fs::Permissions::from_mode(0o700))?;
        }
        app.manage(Arc::new(
            myvault_app_service::AppService::with_app_data_root(app_data),
        ));
        app.manage(app_commands::DesktopVaultWatcher::default());
        Ok(())
    });

    builder
        .invoke_handler(tauri::generate_handler![
            get_platform_info,
            sync_commands::sync_status,
            sync_commands::sync_connect,
            sync_commands::sync_list_folders,
            sync_commands::sync_bind_root,
            sync_commands::sync_scan_step,
            sync_commands::sync_preview,
            sync_commands::sync_run_guarded,
            sync_commands::sync_disconnect,
            app_commands::vault_status,
            app_commands::vault_read_note,
            app_commands::vault_save_note,
            app_commands::vault_list_trash,
            app_commands::vault_list_explorer,
            app_commands::vault_choose_folder
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
}
