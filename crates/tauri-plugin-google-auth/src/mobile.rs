#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use serde::de::DeserializeOwned;
use tauri::{plugin::PluginApi, AppHandle, Runtime};

#[cfg(target_os = "android")]
use tauri::plugin::PluginHandle;

#[cfg(target_os = "android")]
use crate::models::{
    AccessToken, Authorization, AuthorizeRequest, DisconnectRequest, NativeAuthorization,
    NativeDisconnectResult,
};

#[cfg(target_os = "android")]
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<GoogleAuth<R>> {
    let handle = api
        .register_android_plugin("com.abhuri.myvault.googleauth", "GoogleAuthPlugin")
        .map_err(|_| crate::Error::NativeBridge)?;
    Ok(GoogleAuth(handle))
}

#[cfg(target_os = "android")]
pub struct GoogleAuth<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
impl<R: Runtime> GoogleAuth<R> {
    pub fn authorize(&self, scopes: &[&str]) -> crate::Result<Authorization> {
        validate_scopes(scopes)?;
        let response: NativeAuthorization = self
            .0
            .run_mobile_plugin("authorize", AuthorizeRequest { scopes })
            .map_err(|_| crate::Error::NativeBridge)?;

        validate_granted_scopes(&response.granted_scopes)?;

        Ok(Authorization {
            access_token: AccessToken::new(response.access_token),
            granted_scope_count: response.granted_scopes.len(),
        })
    }

    /// Requests a fresh native access token for the one exact R2 scope.
    pub fn fresh_access_token(&self) -> crate::Result<Authorization> {
        self.authorize(&[crate::GOOGLE_DRIVE_SCOPE])
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

#[cfg(not(target_os = "android"))]
pub struct GoogleAuth<R: Runtime>(std::marker::PhantomData<fn() -> R>);

#[cfg(not(target_os = "android"))]
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<GoogleAuth<R>> {
    Err(crate::Error::NativeBridge)
}

fn validate_scopes(scopes: &[&str]) -> crate::Result<()> {
    if scopes == [crate::GOOGLE_DRIVE_SCOPE] {
        Ok(())
    } else {
        Err(crate::Error::InvalidScopes)
    }
}

fn validate_granted_scopes(scopes: &[String]) -> crate::Result<()> {
    let mut observed = scopes.iter().map(String::as_str).collect::<Vec<_>>();
    observed.sort_unstable();
    observed.dedup();
    let exact = [crate::GOOGLE_DRIVE_SCOPE];
    let mut redundant = [
        crate::GOOGLE_DRIVE_SCOPE,
        crate::GOOGLE_DRIVE_METADATA_READONLY_SCOPE,
    ];
    redundant.sort_unstable();
    if observed.len() == scopes.len() && (observed.as_slice() == exact || observed == redundant) {
        Ok(())
    } else {
        Err(crate::Error::InvalidScopes)
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_granted_scopes, validate_scopes};

    #[test]
    fn scope_allowlist_requires_exactly_one_full_drive_scope() {
        assert!(validate_scopes(&[crate::GOOGLE_DRIVE_SCOPE]).is_ok());
        assert!(validate_scopes(&[]).is_err());
        assert!(validate_scopes(&[crate::GOOGLE_DRIVE_SCOPE, crate::GOOGLE_DRIVE_SCOPE]).is_err());
        assert!(
            validate_scopes(&["https://www.googleapis.com/auth/drive.metadata.readonly"]).is_err()
        );
        assert!(validate_scopes(&[crate::GOOGLE_DRIVE_SCOPE, "openid"]).is_err());
    }

    #[test]
    fn native_grants_allow_only_full_drive_and_one_redundant_subset() {
        assert!(validate_granted_scopes(&[crate::GOOGLE_DRIVE_SCOPE.to_owned()]).is_ok());
        assert!(validate_granted_scopes(&[
            crate::GOOGLE_DRIVE_METADATA_READONLY_SCOPE.to_owned(),
            crate::GOOGLE_DRIVE_SCOPE.to_owned(),
        ])
        .is_ok());
        assert!(validate_granted_scopes(&[
            crate::GOOGLE_DRIVE_SCOPE.to_owned(),
            crate::GOOGLE_DRIVE_SCOPE.to_owned(),
        ])
        .is_err());
        assert!(validate_granted_scopes(&[
            crate::GOOGLE_DRIVE_SCOPE.to_owned(),
            "openid".to_owned(),
        ])
        .is_err());
    }
}
