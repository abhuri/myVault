#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Access tokens never implement `Debug`, `Display`, or `Serialize`, so they
/// cannot accidentally enter frontend payloads or ordinary diagnostic logs.
pub struct AccessToken(Zeroizing<String>);

impl AccessToken {
    pub(crate) fn new(value: Zeroizing<String>) -> Self {
        Self(value)
    }

    pub(crate) fn expose_to_native(&self) -> &str {
        self.0.as_str()
    }
}

pub struct Authorization {
    pub access_token: AccessToken,
    pub granted_scope_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthorizeRequest<'a> {
    pub scopes: &'a [&'a str],
}

/// This type is private, deserialize-only, and intentionally has no `Debug`
/// implementation because it is the one native bridge payload containing a
/// bearer token.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeAuthorization {
    pub access_token: Zeroizing<String>,
    pub granted_scopes: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DisconnectRequest<'a> {
    pub access_token: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeDisconnectResult {
    pub revoked: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_bearing_native_types_are_not_debuggable_or_serializable() {
        static_assertions::assert_not_impl_any!(AccessToken: std::fmt::Debug, std::fmt::Display, serde::Serialize);
        static_assertions::assert_not_impl_any!(Authorization: std::fmt::Debug, serde::Serialize);
        static_assertions::assert_not_impl_any!(NativeAuthorization: std::fmt::Debug, serde::Serialize);
    }

    #[test]
    fn authorization_request_serializes_only_the_scope() {
        let request = AuthorizeRequest {
            scopes: &[crate::GOOGLE_DRIVE_SCOPE],
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "scopes": [crate::GOOGLE_DRIVE_SCOPE] })
        );
    }
}
