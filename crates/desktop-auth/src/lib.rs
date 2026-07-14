//! Desktop-only OAuth primitives and native refresh-token storage.
//!
//! Secret-bearing types deliberately implement redacted `Debug` output. Keep
//! this crate behind the native boundary; do not serialize its values to the UI.

#![forbid(unsafe_code)]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use reqwest::{
    blocking::{Client, Response},
    redirect::Policy,
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use url::Url;
use zeroize::Zeroizing;

const CALLBACK_PATH: &str = "/oauth/callback";
const GOOGLE_AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
pub const GOOGLE_DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.metadata.readonly";

/// OAuth errors are intentionally free of secret-bearing response bodies.
#[derive(Debug)]
pub enum AuthError {
    Io(std::io::Error),
    Url(url::ParseError),
    TimedOut,
    InvalidRequest(&'static str),
    InvalidState,
    ProviderRejected { code: String },
    ExchangeFailed(&'static str),
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "loopback I/O failed: {error}"),
            Self::Url(error) => write!(formatter, "OAuth URL is invalid: {error}"),
            Self::TimedOut => formatter.write_str("OAuth callback timed out"),
            Self::InvalidRequest(reason) => write!(formatter, "invalid OAuth callback: {reason}"),
            Self::InvalidState => formatter.write_str("OAuth callback state did not match"),
            Self::ProviderRejected { code } => {
                write!(formatter, "OAuth provider rejected the request ({code})")
            }
            Self::ExchangeFailed(reason) => write!(formatter, "token exchange failed: {reason}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<std::io::Error> for AuthError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<url::ParseError> for AuthError {
    fn from(value: url::ParseError) -> Self {
        Self::Url(value)
    }
}

/// An authorization code is sensitive and is always redacted in diagnostics.
pub struct AuthorizationCode(SecretString);

impl AuthorizationCode {
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for AuthorizationCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizationCode([REDACTED])")
    }
}

/// OAuth client credential used only inside the native token endpoint client.
/// Desktop applications are public clients, but this value must still stay out
/// of repository history, UI DTOs, and diagnostics.
pub struct GoogleClientSecret(SecretString);

impl GoogleClientSecret {
    pub fn parse(value: String) -> Result<Self, AuthError> {
        if value.is_empty()
            || value.len() > 512
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(AuthError::InvalidRequest("client_secret is invalid"));
        }
        Ok(Self(SecretString::from(value)))
    }

    fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for GoogleClientSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GoogleClientSecret([REDACTED])")
    }
}

/// Request data needed by a native token endpoint client.
pub struct TokenExchangeRequest {
    pub client_id: String,
    pub redirect_uri: Url,
    pub code: AuthorizationCode,
    code_verifier: SecretString,
}

/// Request data for obtaining a fresh access token without exposing a client
/// secret. Installed-app OAuth clients authenticate with their client ID and
/// refresh token only.
pub struct TokenRefreshRequest {
    pub client_id: String,
    refresh_token: SecretString,
}

impl TokenRefreshRequest {
    pub fn new(client_id: impl Into<String>, refresh_token: SecretString) -> Self {
        Self {
            client_id: client_id.into(),
            refresh_token,
        }
    }

    pub fn refresh_token(&self) -> &str {
        self.refresh_token.expose_secret()
    }
}

impl fmt::Debug for TokenRefreshRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenRefreshRequest")
            .field("client_id", &self.client_id)
            .field("refresh_token", &"[REDACTED]")
            .finish()
    }
}

impl TokenExchangeRequest {
    pub fn code_verifier(&self) -> &str {
        self.code_verifier.expose_secret()
    }
}

impl fmt::Debug for TokenExchangeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenExchangeRequest")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("code", &self.code)
            .field("code_verifier", &"[REDACTED]")
            .finish()
    }
}

/// Native-layer token result. Both tokens remain opaque to `Debug` and the UI.
pub struct TokenSet {
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
    pub expires_in: Option<Duration>,
}

impl TokenSet {
    /// Borrows the bearer token exclusively for native provider integration.
    pub fn access_token(&self) -> &str {
        self.access_token.expose_secret()
    }
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Keeps HTTP details replaceable and makes tests independent of Google.
pub trait TokenExchanger {
    fn exchange(&self, request: &TokenExchangeRequest) -> Result<TokenSet, AuthError>;
}

/// Refresh is separate from authorization-code exchange so native callers can
/// request a fresh bearer token without retaining the one-time code or PKCE
/// verifier.
pub trait TokenRefresher {
    fn refresh(&self, request: &TokenRefreshRequest) -> Result<TokenSet, AuthError>;
}

/// Concrete Google OAuth token endpoint client for installed desktop apps.
/// Redirects are disabled and provider response bodies never enter errors.
pub struct GoogleTokenClient {
    client: Client,
    endpoint: Url,
    client_secret: GoogleClientSecret,
}

impl fmt::Debug for GoogleTokenClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleTokenClient")
            .field("endpoint", &self.endpoint)
            .field("client_secret", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl GoogleTokenClient {
    pub fn new(client_secret: GoogleClientSecret) -> Result<Self, AuthError> {
        Self::with_endpoint(Url::parse(GOOGLE_TOKEN_ENDPOINT)?, client_secret)
    }

    fn with_endpoint(endpoint: Url, client_secret: GoogleClientSecret) -> Result<Self, AuthError> {
        Self::with_endpoint_and_timeouts(
            endpoint,
            client_secret,
            Duration::from_secs(10),
            Duration::from_secs(30),
        )
    }

    fn with_endpoint_and_timeouts(
        endpoint: Url,
        client_secret: GoogleClientSecret,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, AuthError> {
        let client = Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .redirect(Policy::none())
            .build()
            .map_err(|_| AuthError::ExchangeFailed("HTTP client initialization failed"))?;
        Ok(Self {
            client,
            endpoint,
            client_secret,
        })
    }

    fn send_form(&self, form: &[(&str, &str)]) -> Result<TokenSet, AuthError> {
        let mut form = form.to_vec();
        form.push(("client_secret", self.client_secret.expose()));
        let response = self
            .client
            .post(self.endpoint.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .map_err(|error| {
                if error.is_timeout() {
                    AuthError::ExchangeFailed("token endpoint timed out")
                } else if error.is_redirect() {
                    AuthError::ExchangeFailed("token endpoint redirect rejected")
                } else {
                    AuthError::ExchangeFailed("token endpoint request failed")
                }
            })?;
        parse_token_response(response)
    }
}

impl TokenExchanger for GoogleTokenClient {
    fn exchange(&self, request: &TokenExchangeRequest) -> Result<TokenSet, AuthError> {
        self.send_form(&[
            ("client_id", request.client_id.as_str()),
            ("code", request.code.expose()),
            ("code_verifier", request.code_verifier()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", request.redirect_uri.as_str()),
        ])
    }
}

impl TokenRefresher for GoogleTokenClient {
    fn refresh(&self, request: &TokenRefreshRequest) -> Result<TokenSet, AuthError> {
        self.send_form(&[
            ("client_id", request.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", request.refresh_token()),
        ])
    }
}

#[derive(Deserialize)]
struct TokenEndpointResponse {
    access_token: SecretString,
    #[serde(default)]
    refresh_token: Option<SecretString>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

fn parse_token_response(mut response: Response) -> Result<TokenSet, AuthError> {
    if !response.status().is_success() {
        return Err(AuthError::ExchangeFailed("token endpoint rejected request"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TOKEN_RESPONSE_BYTES as u64)
    {
        return Err(AuthError::ExchangeFailed(
            "token endpoint response too large",
        ));
    }

    let mut body = Zeroizing::new(Vec::with_capacity(1024));
    response
        .by_ref()
        .take((MAX_TOKEN_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|_| AuthError::ExchangeFailed("token endpoint response read failed"))?;
    if body.len() > MAX_TOKEN_RESPONSE_BYTES {
        return Err(AuthError::ExchangeFailed(
            "token endpoint response too large",
        ));
    }

    let payload: TokenEndpointResponse = serde_json::from_slice(body.as_slice())
        .map_err(|_| AuthError::ExchangeFailed("token endpoint response was invalid"))?;
    if payload.access_token.expose_secret().trim().is_empty() {
        return Err(AuthError::ExchangeFailed("access token was missing"));
    }
    if payload
        .token_type
        .as_deref()
        .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
    {
        return Err(AuthError::ExchangeFailed("token type was not bearer"));
    }
    if payload
        .scope
        .as_deref()
        .is_some_and(|value| value.split_whitespace().collect::<Vec<_>>() != [GOOGLE_DRIVE_SCOPE])
    {
        return Err(AuthError::ExchangeFailed(
            "granted scope did not match the allowlist",
        ));
    }

    Ok(TokenSet {
        access_token: payload.access_token,
        refresh_token: payload.refresh_token,
        expires_in: payload.expires_in.map(Duration::from_secs),
    })
}

pub struct DesktopOAuth {
    listener: TcpListener,
    client_id: String,
    authorization_url: Url,
    redirect_uri: Url,
    expected_state: SecretString,
    code_verifier: SecretString,
}

impl DesktopOAuth {
    /// Binds only to IPv4 loopback and lets the OS select an unused port.
    pub fn bind(client_id: impl Into<String>, scopes: &[&str]) -> Result<Self, AuthError> {
        let client_id = client_id.into();
        if client_id.trim().is_empty() {
            return Err(AuthError::InvalidRequest("client_id is empty"));
        }
        if scopes != [GOOGLE_DRIVE_SCOPE] {
            return Err(AuthError::InvalidRequest(
                "requested scopes do not match the native Drive scope allowlist",
            ));
        }

        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let redirect_uri = Url::parse(&format!("http://127.0.0.1:{port}{CALLBACK_PATH}"))?;
        let state = random_base64url(32);
        let verifier = random_base64url(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

        let mut authorization_url = Url::parse(GOOGLE_AUTHORIZATION_ENDPOINT)?;
        authorization_url
            .query_pairs_mut()
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", redirect_uri.as_str())
            .append_pair("response_type", "code")
            .append_pair("scope", &scopes.join(" "))
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");

        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            client_id,
            authorization_url,
            redirect_uri,
            expected_state: SecretString::from(state),
            code_verifier: SecretString::from(verifier),
        })
    }

    pub fn authorization_url(&self) -> &Url {
        &self.authorization_url
    }

    pub fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    /// Waits for one valid callback. Malformed or unrelated requests receive an
    /// error response and do not extend the original deadline.
    pub fn wait_for_callback(self, timeout: Duration) -> Result<TokenExchangeRequest, AuthError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.listener.accept() {
                Ok((mut stream, _peer)) => {
                    let result = parse_stream_callback(
                        &mut stream,
                        self.expected_state.expose_secret(),
                        deadline,
                    );
                    let success = result.is_ok();
                    write_browser_response(&mut stream, success);
                    match result {
                        Ok(code) => {
                            return Ok(TokenExchangeRequest {
                                client_id: self.client_id,
                                redirect_uri: self.redirect_uri,
                                code,
                                code_verifier: self.code_verifier,
                            });
                        }
                        Err(error @ AuthError::ProviderRejected { .. }) => return Err(error),
                        Err(_) => {
                            // Ignore malformed probes and wrong-state callbacks so a
                            // legitimate browser callback can still arrive before the
                            // original deadline.
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(AuthError::TimedOut);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(AuthError::Io(error)),
            }
        }
    }
}

fn random_base64url(bytes: usize) -> String {
    let mut random = vec![0_u8; bytes];
    rand::rng().fill_bytes(&mut random);
    URL_SAFE_NO_PAD.encode(random)
}

fn parse_stream_callback(
    stream: &mut TcpStream,
    expected_state: &str,
    deadline: Instant,
) -> Result<AuthorizationCode, AuthError> {
    // Accepted streams can inherit the listener's nonblocking mode on some
    // platforms. Switch to deadline-bounded blocking reads so fragmented
    // browser requests are assembled instead of being rejected on WouldBlock.
    stream.set_nonblocking(false)?;
    let mut request_bytes = Vec::with_capacity(1024);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(AuthError::TimedOut)?;
        if remaining.is_zero() {
            return Err(AuthError::TimedOut);
        }
        stream.set_read_timeout(Some(remaining))?;

        let available = MAX_REQUEST_BYTES - request_bytes.len();
        if available == 0 {
            return Err(AuthError::InvalidRequest("request headers exceeded 16 KiB"));
        }
        let mut chunk = [0_u8; 1024];
        let read_limit = available.min(chunk.len());
        let count = match stream.read(&mut chunk[..read_limit]) {
            Ok(0) => return Err(AuthError::InvalidRequest("request headers were incomplete")),
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(AuthError::TimedOut);
            }
            Err(error) => return Err(AuthError::Io(error)),
        };
        request_bytes.extend_from_slice(&chunk[..count]);
        if request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let request = std::str::from_utf8(&request_bytes)
        .map_err(|_| AuthError::InvalidRequest("request was not UTF-8"))?;
    let request_line = request
        .lines()
        .next()
        .ok_or(AuthError::InvalidRequest("request line missing"))?;
    let mut fields = request_line.split_whitespace();
    if fields.next() != Some("GET") {
        return Err(AuthError::InvalidRequest("only GET is accepted"));
    }
    let target = fields
        .next()
        .ok_or(AuthError::InvalidRequest("request target missing"))?;
    parse_callback_target(target, expected_state)
}

fn parse_callback_target(
    target: &str,
    expected_state: &str,
) -> Result<AuthorizationCode, AuthError> {
    let callback = Url::parse(&format!("http://127.0.0.1{target}"))?;
    if callback.path() != CALLBACK_PATH {
        return Err(AuthError::InvalidRequest("callback path did not match"));
    }

    let parameters: HashMap<_, _> = callback.query_pairs().into_owned().collect();
    let state = parameters
        .get("state")
        .ok_or(AuthError::InvalidRequest("state missing"))?;
    if !bool::from(state.as_bytes().ct_eq(expected_state.as_bytes())) {
        return Err(AuthError::InvalidState);
    }
    if let Some(code) = parameters.get("error") {
        return Err(AuthError::ProviderRejected {
            code: bounded_provider_error_code(code),
        });
    }
    let code = parameters
        .get("code")
        .filter(|code| !code.is_empty())
        .ok_or(AuthError::InvalidRequest("authorization code missing"))?;
    Ok(AuthorizationCode(SecretString::from(code.clone())))
}

fn bounded_provider_error_code(code: &str) -> String {
    if !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        code.to_owned()
    } else {
        "provider_error".to_owned()
    }
}

fn write_browser_response(stream: &mut TcpStream, success: bool) {
    let (status, body) = if success {
        (
            "200 OK",
            "Authorization complete. You may close this window.",
        )
    } else {
        ("400 Bad Request", "Authorization callback was rejected.")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[derive(Debug)]
pub enum SecretStoreError {
    BackendUnavailable,
    BackendFailure,
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendUnavailable => {
                formatter.write_str("secure credential storage is unavailable")
            }
            Self::BackendFailure => {
                formatter.write_str("secure credential storage operation failed")
            }
        }
    }
}

impl std::error::Error for SecretStoreError {}

pub trait SecretStore {
    fn save_refresh_token(
        &self,
        account: &str,
        token: &SecretString,
    ) -> Result<(), SecretStoreError>;
    fn load_refresh_token(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError>;
    fn delete_refresh_token(&self, account: &str) -> Result<(), SecretStoreError>;
}

/// Uses Keychain on macOS, Credential Manager on Windows, and Secret Service
/// on Linux. Backend errors are deliberately collapsed to avoid leaking data.
pub struct OsKeyringStore {
    service: String,
}

impl OsKeyringStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry, SecretStoreError> {
        keyring::Entry::new(&self.service, account).map_err(map_keyring_error)
    }
}

impl SecretStore for OsKeyringStore {
    fn save_refresh_token(
        &self,
        account: &str,
        token: &SecretString,
    ) -> Result<(), SecretStoreError> {
        self.entry(account)?
            .set_password(token.expose_secret())
            .map_err(map_keyring_error)
    }

    fn load_refresh_token(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError> {
        match self.entry(account)?.get_password() {
            Ok(token) => Ok(Some(SecretString::from(token))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(map_keyring_error(error)),
        }
    }

    fn delete_refresh_token(&self, account: &str) -> Result<(), SecretStoreError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(map_keyring_error(error)),
        }
    }
}

fn map_keyring_error(error: keyring::Error) -> SecretStoreError {
    match error {
        keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_) => {
            SecretStoreError::BackendUnavailable
        }
        _ => SecretStoreError::BackendFailure,
    }
}

/// Test double that preserves the same secret-bearing API as the OS adapter.
#[derive(Default)]
pub struct InMemorySecretStore {
    values: Mutex<HashMap<String, SecretString>>,
}

impl SecretStore for InMemorySecretStore {
    fn save_refresh_token(
        &self,
        account: &str,
        token: &SecretString,
    ) -> Result<(), SecretStoreError> {
        self.values
            .lock()
            .map_err(|_| SecretStoreError::BackendFailure)?
            .insert(
                account.to_owned(),
                SecretString::from(token.expose_secret().to_owned()),
            );
        Ok(())
    }

    fn load_refresh_token(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError> {
        let values = self
            .values
            .lock()
            .map_err(|_| SecretStoreError::BackendFailure)?;
        Ok(values
            .get(account)
            .map(|token| SecretString::from(token.expose_secret().to_owned())))
    }

    fn delete_refresh_token(&self, account: &str) -> Result<(), SecretStoreError> {
        self.values
            .lock()
            .map_err(|_| SecretStoreError::BackendFailure)?
            .remove(account);
        Ok(())
    }
}

/// Access token returned only to native provider code. Diagnostics are
/// redacted and the type deliberately has no serialization implementation.
pub struct FreshAccessToken {
    value: SecretString,
    pub expires_in: Option<Duration>,
}

impl FreshAccessToken {
    pub fn expose_to_native(&self) -> &str {
        self.value.expose_secret()
    }
}

impl fmt::Debug for FreshAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FreshAccessToken")
            .field("value", &"[REDACTED]")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Debug)]
pub enum NativeTokenProviderError {
    Auth(AuthError),
    SecretStore(SecretStoreError),
    InvalidAccountIdentity,
    RefreshTokenMissing,
}

impl fmt::Display for NativeTokenProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(error) => write!(formatter, "native token operation failed: {error}"),
            Self::SecretStore(error) => {
                write!(formatter, "native credential operation failed: {error}")
            }
            Self::InvalidAccountIdentity => {
                formatter.write_str("provider account identity is invalid")
            }
            Self::RefreshTokenMissing => formatter.write_str("refresh token is unavailable"),
        }
    }
}

impl std::error::Error for NativeTokenProviderError {}

impl From<AuthError> for NativeTokenProviderError {
    fn from(value: AuthError) -> Self {
        Self::Auth(value)
    }
}

impl From<SecretStoreError> for NativeTokenProviderError {
    fn from(value: SecretStoreError) -> Self {
        Self::SecretStore(value)
    }
}

/// Coordinates token exchange/refresh with native secure storage. Account IDs
/// are provider-stable opaque identities, not display names or email addresses.
pub struct NativeTokenProvider<E, S> {
    client_id: String,
    endpoint: E,
    store: S,
}

impl<E, S> NativeTokenProvider<E, S>
where
    E: TokenExchanger + TokenRefresher,
    S: SecretStore,
{
    pub fn new(
        client_id: impl Into<String>,
        endpoint: E,
        store: S,
    ) -> Result<Self, NativeTokenProviderError> {
        let client_id = client_id.into();
        if client_id.trim().is_empty() {
            return Err(NativeTokenProviderError::Auth(AuthError::InvalidRequest(
                "client_id is empty",
            )));
        }
        Ok(Self {
            client_id,
            endpoint,
            store,
        })
    }

    pub fn exchange(
        &self,
        request: &TokenExchangeRequest,
    ) -> Result<TokenSet, NativeTokenProviderError> {
        if request.client_id != self.client_id {
            return Err(NativeTokenProviderError::Auth(AuthError::InvalidRequest(
                "client_id did not match native provider",
            )));
        }
        Ok(self.endpoint.exchange(request)?)
    }

    pub fn save_refresh_token(
        &self,
        account_id: &str,
        token: &SecretString,
    ) -> Result<(), NativeTokenProviderError> {
        validate_account_identity(account_id)?;
        self.store.save_refresh_token(account_id, token)?;
        Ok(())
    }

    pub fn fresh_access_token(
        &self,
        account_id: &str,
    ) -> Result<FreshAccessToken, NativeTokenProviderError> {
        validate_account_identity(account_id)?;
        let refresh_token = self
            .store
            .load_refresh_token(account_id)?
            .ok_or(NativeTokenProviderError::RefreshTokenMissing)?;
        let refreshed = self.endpoint.refresh(&TokenRefreshRequest::new(
            self.client_id.clone(),
            refresh_token,
        ))?;

        if let Some(rotated_refresh_token) = refreshed.refresh_token.as_ref() {
            self.store
                .save_refresh_token(account_id, rotated_refresh_token)?;
        }
        Ok(FreshAccessToken {
            value: refreshed.access_token,
            expires_in: refreshed.expires_in,
        })
    }

    pub fn disconnect(&self, account_id: &str) -> Result<(), NativeTokenProviderError> {
        validate_account_identity(account_id)?;
        self.store.delete_refresh_token(account_id)?;
        Ok(())
    }
}

fn validate_account_identity(account_id: &str) -> Result<(), NativeTokenProviderError> {
    if (1..=512).contains(&account_id.len())
        && account_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(NativeTokenProviderError::InvalidAccountIdentity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use std::net::TcpStream;

    struct MockExchanger;

    impl TokenExchanger for MockExchanger {
        fn exchange(&self, request: &TokenExchangeRequest) -> Result<TokenSet, AuthError> {
            assert_eq!(request.code.expose(), "one-time-code");
            assert!(request.code_verifier().len() >= 43);
            Ok(TokenSet {
                access_token: SecretString::from("access-secret".to_owned()),
                refresh_token: Some(SecretString::from("refresh-secret".to_owned())),
                expires_in: Some(Duration::from_secs(3600)),
            })
        }
    }

    #[test]
    fn url_uses_literal_loopback_pkce_and_state() {
        let flow = DesktopOAuth::bind("test-client", &[GOOGLE_DRIVE_SCOPE]).unwrap();
        assert_eq!(flow.redirect_uri().host_str(), Some("127.0.0.1"));
        assert_ne!(flow.redirect_uri().port(), Some(0));
        let query: HashMap<_, _> = flow
            .authorization_url()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("scope").map(String::as_str),
            Some(GOOGLE_DRIVE_SCOPE)
        );
        assert!(query.get("state").is_some_and(|state| state.len() >= 43));
        assert!(query
            .get("code_challenge")
            .is_some_and(|value| value.len() == 43));
        assert!(!query.contains_key("client_secret"));
    }

    #[test]
    fn native_boundary_rejects_scopes_outside_exact_drive_allowlist() {
        let wrong_scope = DesktopOAuth::bind(
            "test-client",
            &["https://www.googleapis.com/auth/drive.file"],
        )
        .err()
        .unwrap();
        assert!(matches!(wrong_scope, AuthError::InvalidRequest(_)));

        let extra_scope = DesktopOAuth::bind("test-client", &[GOOGLE_DRIVE_SCOPE, "openid"])
            .err()
            .unwrap();
        assert!(matches!(extra_scope, AuthError::InvalidRequest(_)));
    }

    #[test]
    fn callback_validates_state_and_mock_exchange() {
        let flow = DesktopOAuth::bind("test-client", &[GOOGLE_DRIVE_SCOPE]).unwrap();
        let state = flow
            .authorization_url()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let address = ("127.0.0.1", flow.redirect_uri().port().unwrap());
        let sender = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET {CALLBACK_PATH}?code=one-time-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        });
        let request = flow.wait_for_callback(Duration::from_secs(2)).unwrap();
        sender.join().unwrap();
        let tokens = MockExchanger.exchange(&request).unwrap();
        assert_eq!(tokens.access_token.expose_secret(), "access-secret");
    }

    #[test]
    fn callback_parser_handles_fragmented_http_headers() {
        let flow = DesktopOAuth::bind("test-client", &[GOOGLE_DRIVE_SCOPE]).unwrap();
        let state = flow
            .authorization_url()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let address = ("127.0.0.1", flow.redirect_uri().port().unwrap());
        let sender = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(b"GET /oauth/").unwrap();
            thread::sleep(Duration::from_millis(5));
            stream
                .write_all(b"callback?code=fragmented-code&state=")
                .unwrap();
            thread::sleep(Duration::from_millis(5));
            stream.write_all(state.as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(5));
            stream
                .write_all(b" HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .unwrap();
        });

        let request = flow.wait_for_callback(Duration::from_secs(2)).unwrap();
        sender.join().unwrap();
        assert_eq!(request.code.expose(), "fragmented-code");
    }

    #[test]
    fn callback_rejects_wrong_state_then_times_out() {
        let flow = DesktopOAuth::bind("test-client", &[GOOGLE_DRIVE_SCOPE]).unwrap();
        let address = ("127.0.0.1", flow.redirect_uri().port().unwrap());
        let sender = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "GET {CALLBACK_PATH}?code=stolen&state=wrong HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
            )
            .unwrap();
        });
        let error = flow
            .wait_for_callback(Duration::from_millis(100))
            .unwrap_err();
        sender.join().unwrap();
        assert!(matches!(error, AuthError::TimedOut));
    }

    #[test]
    fn callback_wait_has_a_timeout() {
        let flow = DesktopOAuth::bind("test-client", &[GOOGLE_DRIVE_SCOPE]).unwrap();
        let error = flow
            .wait_for_callback(Duration::from_millis(20))
            .unwrap_err();
        assert!(matches!(error, AuthError::TimedOut));
    }

    #[test]
    fn provider_denial_with_valid_state_is_returned_immediately() {
        let flow = DesktopOAuth::bind("test-client", &[GOOGLE_DRIVE_SCOPE]).unwrap();
        let state = flow
            .authorization_url()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let address = ("127.0.0.1", flow.redirect_uri().port().unwrap());
        let sender = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "GET {CALLBACK_PATH}?error=access_denied&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
            )
            .unwrap();
        });
        let error = flow.wait_for_callback(Duration::from_secs(2)).unwrap_err();
        sender.join().unwrap();
        assert!(matches!(
            error,
            AuthError::ProviderRejected { code } if code == "access_denied"
        ));
    }

    #[test]
    fn provider_denial_code_is_bounded_and_sanitized() {
        let secret = "provider-secret-that-must-not-enter-diagnostics";
        let error = parse_callback_target(
            &format!("{CALLBACK_PATH}?error={secret}%2Finvalid&state=expected"),
            "expected",
        )
        .unwrap_err();
        let diagnostic = format!("{error:?} {error}");
        assert!(matches!(
            error,
            AuthError::ProviderRejected { ref code } if code == "provider_error"
        ));
        assert!(!diagnostic.contains(secret));
    }

    #[test]
    fn secret_debug_output_is_redacted() {
        let request = TokenExchangeRequest {
            client_id: "client".to_owned(),
            redirect_uri: Url::parse("http://127.0.0.1:1234/oauth/callback").unwrap(),
            code: AuthorizationCode(SecretString::from("authorization-secret".to_owned())),
            code_verifier: SecretString::from("verifier-secret".to_owned()),
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("authorization-secret"));
        assert!(!debug.contains("verifier-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn in_memory_store_supports_save_load_and_idempotent_logout() {
        let store = InMemorySecretStore::default();
        let token = SecretString::from("refresh-secret".to_owned());
        store
            .save_refresh_token("user@example.com", &token)
            .unwrap();
        let loaded = store
            .load_refresh_token("user@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.expose_secret(), "refresh-secret");
        store.delete_refresh_token("user@example.com").unwrap();
        store.delete_refresh_token("user@example.com").unwrap();
        assert!(store
            .load_refresh_token("user@example.com")
            .unwrap()
            .is_none());
    }

    fn exchange_request() -> TokenExchangeRequest {
        TokenExchangeRequest {
            client_id: "desktop-client".to_owned(),
            redirect_uri: Url::parse("http://127.0.0.1:49152/oauth/callback").unwrap(),
            code: AuthorizationCode(SecretString::from("one-time-secret-code".to_owned())),
            code_verifier: SecretString::from("pkce-secret-verifier".to_owned()),
        }
    }

    fn test_client_secret() -> GoogleClientSecret {
        GoogleClientSecret::parse("desktop-client-secret".to_owned()).unwrap()
    }

    #[test]
    fn google_exchange_posts_installed_app_fields_with_client_secret() {
        let mut server = Server::new();
        let request = exchange_request();
        let exchange = server
            .mock("POST", "/token")
            .match_header("accept", "application/json")
            .match_body(Matcher::AllOf(vec![
                Matcher::UrlEncoded("client_id".into(), "desktop-client".into()),
                Matcher::UrlEncoded("client_secret".into(), "desktop-client-secret".into()),
                Matcher::UrlEncoded("code".into(), "one-time-secret-code".into()),
                Matcher::UrlEncoded("code_verifier".into(), "pkce-secret-verifier".into()),
                Matcher::UrlEncoded("grant_type".into(), "authorization_code".into()),
                Matcher::UrlEncoded(
                    "redirect_uri".into(),
                    request.redirect_uri.as_str().into(),
                ),
            ]))
            .with_status(200)
            .with_body(format!(
                r#"{{"access_token":"access-secret","refresh_token":"refresh-secret","expires_in":3600,"token_type":"Bearer","scope":"{GOOGLE_DRIVE_SCOPE}"}}"#
            ))
            .create();
        let endpoint = Url::parse(&format!("{}/token", server.url())).unwrap();
        let client = GoogleTokenClient::with_endpoint(endpoint, test_client_secret()).unwrap();

        let tokens = client.exchange(&request).unwrap();

        exchange.assert();
        assert_eq!(tokens.access_token.expose_secret(), "access-secret");
        assert_eq!(
            tokens.refresh_token.unwrap().expose_secret(),
            "refresh-secret"
        );
        assert_eq!(tokens.expires_in, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn google_refresh_posts_client_secret_and_refresh_grant_fields() {
        let mut server = Server::new();
        let refresh = server
            .mock("POST", "/token")
            .match_body(Matcher::AllOf(vec![
                Matcher::UrlEncoded("client_id".into(), "desktop-client".into()),
                Matcher::UrlEncoded("client_secret".into(), "desktop-client-secret".into()),
                Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
                Matcher::UrlEncoded("refresh_token".into(), "refresh-secret".into()),
            ]))
            .with_status(200)
            .with_body(r#"{"access_token":"fresh-access","expires_in":1200,"token_type":"bearer"}"#)
            .create();
        let endpoint = Url::parse(&format!("{}/token", server.url())).unwrap();
        let client = GoogleTokenClient::with_endpoint(endpoint, test_client_secret()).unwrap();
        let request = TokenRefreshRequest::new(
            "desktop-client",
            SecretString::from("refresh-secret".to_owned()),
        );

        let tokens = client.refresh(&request).unwrap();

        refresh.assert();
        assert_eq!(tokens.access_token.expose_secret(), "fresh-access");
        assert!(tokens.refresh_token.is_none());
    }

    #[test]
    fn token_endpoint_redirect_and_provider_body_are_rejected_without_leaking_body() {
        let mut server = Server::new();
        let redirect = server
            .mock("POST", "/token")
            .with_status(302)
            .with_header("location", "/provider-secret-body")
            .with_body("provider-secret-body")
            .create();
        let endpoint = Url::parse(&format!("{}/token", server.url())).unwrap();
        let client = GoogleTokenClient::with_endpoint(endpoint, test_client_secret()).unwrap();

        let error = client.exchange(&exchange_request()).unwrap_err();

        redirect.assert();
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("provider-secret-body"));
        assert!(matches!(error, AuthError::ExchangeFailed(_)));
    }

    #[test]
    fn token_endpoint_timeout_is_bounded_and_redacted() {
        let mut server = Server::new();
        let _slow = server
            .mock("POST", "/token")
            .with_status(200)
            .with_chunked_body(|writer| {
                thread::sleep(Duration::from_millis(100));
                writer.write_all(br#"{"access_token":"too-late"}"#)
            })
            .create();
        let endpoint = Url::parse(&format!("{}/token", server.url())).unwrap();
        let client = GoogleTokenClient::with_endpoint_and_timeouts(
            endpoint,
            test_client_secret(),
            Duration::from_millis(20),
            Duration::from_millis(20),
        )
        .unwrap();

        let error = client.exchange(&exchange_request()).unwrap_err();

        assert!(matches!(error, AuthError::ExchangeFailed(_)));
        assert!(!format!("{error:?} {error}").contains("too-late"));
    }

    #[test]
    fn token_endpoint_rejects_oversized_or_wrong_scope_responses() {
        let mut oversized_server = Server::new();
        let _oversized = oversized_server
            .mock("POST", "/token")
            .with_status(200)
            .with_body(vec![b'x'; MAX_TOKEN_RESPONSE_BYTES + 1])
            .create();
        let oversized_client = GoogleTokenClient::with_endpoint(
            Url::parse(&format!("{}/token", oversized_server.url())).unwrap(),
            test_client_secret(),
        )
        .unwrap();
        assert!(matches!(
            oversized_client.exchange(&exchange_request()),
            Err(AuthError::ExchangeFailed(
                "token endpoint response too large"
            ))
        ));

        let mut scope_server = Server::new();
        let _wrong_scope = scope_server
            .mock("POST", "/token")
            .with_status(200)
            .with_body(
                r#"{"access_token":"secret","token_type":"Bearer","scope":"https://www.googleapis.com/auth/drive"}"#,
            )
            .create();
        let scope_client = GoogleTokenClient::with_endpoint(
            Url::parse(&format!("{}/token", scope_server.url())).unwrap(),
            test_client_secret(),
        )
        .unwrap();
        assert!(matches!(
            scope_client.exchange(&exchange_request()),
            Err(AuthError::ExchangeFailed(
                "granted scope did not match the allowlist"
            ))
        ));
    }

    #[derive(Default)]
    struct RotatingEndpoint;

    impl TokenExchanger for RotatingEndpoint {
        fn exchange(&self, _request: &TokenExchangeRequest) -> Result<TokenSet, AuthError> {
            Ok(TokenSet {
                access_token: SecretString::from("initial-access".to_owned()),
                refresh_token: Some(SecretString::from("initial-refresh".to_owned())),
                expires_in: Some(Duration::from_secs(60)),
            })
        }
    }

    impl TokenRefresher for RotatingEndpoint {
        fn refresh(&self, request: &TokenRefreshRequest) -> Result<TokenSet, AuthError> {
            assert_eq!(request.refresh_token(), "initial-refresh");
            Ok(TokenSet {
                access_token: SecretString::from("fresh-access".to_owned()),
                refresh_token: Some(SecretString::from("rotated-refresh".to_owned())),
                expires_in: Some(Duration::from_secs(120)),
            })
        }
    }

    #[test]
    fn native_provider_refreshes_rotates_and_cleans_up_idempotently() {
        let provider = NativeTokenProvider::new(
            "desktop-client",
            RotatingEndpoint,
            InMemorySecretStore::default(),
        )
        .unwrap();
        provider
            .save_refresh_token(
                "opaque-account-id",
                &SecretString::from("initial-refresh".to_owned()),
            )
            .unwrap();

        let token = provider.fresh_access_token("opaque-account-id").unwrap();

        assert_eq!(token.expose_to_native(), "fresh-access");
        assert_eq!(token.expires_in, Some(Duration::from_secs(120)));
        assert_eq!(
            provider
                .store
                .load_refresh_token("opaque-account-id")
                .unwrap()
                .unwrap()
                .expose_secret(),
            "rotated-refresh"
        );
        provider.disconnect("opaque-account-id").unwrap();
        provider.disconnect("opaque-account-id").unwrap();
        assert!(matches!(
            provider.fresh_access_token("opaque-account-id"),
            Err(NativeTokenProviderError::RefreshTokenMissing)
        ));
    }

    #[test]
    fn native_provider_rejects_noncanonical_account_identities() {
        let provider = NativeTokenProvider::new(
            "desktop-client",
            RotatingEndpoint,
            InMemorySecretStore::default(),
        )
        .unwrap();
        for invalid in ["", "account id", " account", "account\nsecret", "บัญชี"] {
            assert!(matches!(
                provider.fresh_access_token(invalid),
                Err(NativeTokenProviderError::InvalidAccountIdentity)
            ));
        }
        assert!(matches!(
            provider.fresh_access_token(&"a".repeat(513)),
            Err(NativeTokenProviderError::InvalidAccountIdentity)
        ));
    }

    #[test]
    fn secret_bearing_types_are_not_serializable_and_debug_is_redacted() {
        static_assertions::assert_not_impl_any!(AuthorizationCode: serde::Serialize);
        static_assertions::assert_not_impl_any!(GoogleClientSecret: serde::Serialize);
        static_assertions::assert_not_impl_any!(GoogleTokenClient: serde::Serialize);
        static_assertions::assert_not_impl_any!(TokenExchangeRequest: serde::Serialize);
        static_assertions::assert_not_impl_any!(TokenRefreshRequest: serde::Serialize);
        static_assertions::assert_not_impl_any!(TokenSet: serde::Serialize);
        static_assertions::assert_not_impl_any!(FreshAccessToken: serde::Serialize);

        let refresh = TokenRefreshRequest::new(
            "desktop-client",
            SecretString::from("refresh-secret".to_owned()),
        );
        let debug = format!("{refresh:?}");
        assert!(!debug.contains("refresh-secret"));
        assert!(debug.contains("[REDACTED]"));

        let secret_value = "desktop-client-secret-that-must-stay-native";
        let client = GoogleTokenClient::with_endpoint(
            Url::parse("https://oauth2.googleapis.com/token").unwrap(),
            GoogleClientSecret::parse(secret_value.to_owned()).unwrap(),
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains(secret_value));
        assert!(debug.contains("[REDACTED]"));

        let fresh = FreshAccessToken {
            value: SecretString::from("access-secret".to_owned()),
            expires_in: None,
        };
        let debug = format!("{fresh:?}");
        assert!(!debug.contains("access-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn google_client_secret_validation_is_fail_closed_and_redacted() {
        let valid = GoogleClientSecret::parse("desktop-client-secret_123".to_owned()).unwrap();
        assert_eq!(format!("{valid:?}"), "GoogleClientSecret([REDACTED])");

        for invalid in ["", "secret value", "secret\nvalue", "บัญชี"] {
            assert!(matches!(
                GoogleClientSecret::parse(invalid.to_owned()),
                Err(AuthError::InvalidRequest("client_secret is invalid"))
            ));
        }
        assert!(matches!(
            GoogleClientSecret::parse("a".repeat(513)),
            Err(AuthError::InvalidRequest("client_secret is invalid"))
        ));
    }
}
