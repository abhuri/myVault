use serde::de::DeserializeOwned;
use tauri::{
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

use crate::models::{
    AccessToken, Authorization, AuthorizeRequest, DisconnectRequest, NativeAuthorization,
    NativeDisconnectResult,
};

pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<GoogleAuth<R>> {
    let handle = api
        .register_android_plugin("com.abhuri.myvault.googleauth", "GoogleAuthPlugin")
        .map_err(|_| crate::Error::NativeBridge)?;
    Ok(GoogleAuth(handle))
}

pub struct GoogleAuth<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> GoogleAuth<R> {
    pub fn authorize(&self, scopes: &[&str]) -> crate::Result<Authorization> {
        let response: NativeAuthorization = self
            .0
            .run_mobile_plugin("authorize", AuthorizeRequest { scopes })
            .map_err(|_| crate::Error::NativeBridge)?;

        Ok(Authorization {
            access_token: AccessToken::new(response.access_token),
            granted_scope_count: response.granted_scopes.len(),
        })
    }

    pub fn disconnect(&self, token: &AccessToken) -> crate::Result<bool> {
        let response: NativeDisconnectResult = self
            .0
            .run_mobile_plugin(
                "disconnect",
                DisconnectRequest {
                    access_token: token.expose_to_native(),
                },
            )
            .map_err(|_| crate::Error::NativeBridge)?;
        Ok(response.revoked)
    }
}
