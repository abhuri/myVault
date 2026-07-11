use serde::Serialize;

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
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_platform_info])
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
