use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Access tokens never implement `Debug`, `Display`, or `Serialize`, so they
/// cannot accidentally enter frontend payloads or ordinary diagnostic logs.
pub struct AccessToken(Zeroizing<String>);

impl AccessToken {
    pub(crate) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
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
    pub access_token: String,
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
