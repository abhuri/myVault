use std::path::{Path, PathBuf};

use serde::Deserialize;
use tauri::{
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

use crate::{NativeNoBackupRoot, PrivateRootError};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeNoBackupPath {
    path: PathBuf,
}

pub fn init<R: Runtime, C: serde::de::DeserializeOwned>(
    _app: &AppHandle<R>,
    api: &PluginApi<R, C>,
) -> Result<PrivateRoot<R>, PrivateRootError> {
    let handle = api
        .register_android_plugin("com.abhuri.myvault.privateroot", "PrivateRootPlugin")
        .map_err(|_| PrivateRootError::NativeBridge)?;
    Ok(PrivateRoot(handle))
}

pub struct PrivateRoot<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> PrivateRoot<R> {
    pub fn claim(&self, vault_root: &Path) -> Result<NativeNoBackupRoot, PrivateRootError> {
        let response: NativeNoBackupPath = self
            .0
            .run_mobile_plugin("noBackupRoot", ())
            .map_err(|_| PrivateRootError::NativeBridge)?;
        let inspected =
            myvault_private_fs::inspect_android_private_root(&response.path, vault_root)
                .map_err(PrivateRootError::Validation)?;
        let acl = inspected.acl_inspection();
        Ok(NativeNoBackupRoot {
            directory: inspected.into_untrusted_directory(),
            acl,
        })
    }
}
