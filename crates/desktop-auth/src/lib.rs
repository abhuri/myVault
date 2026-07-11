//! Desktop-only OAuth primitives and native refresh-token storage.
//!
//! Secret-bearing types deliberately implement redacted `Debug` output. Keep
//! this crate behind the native boundary; do not serialize its values to the UI.

#![forbid(unsafe_code)]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use secrecy::{ExposeSecret, SecretString};
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

const CALLBACK_PATH: &str = "/oauth/callback";
const GOOGLE_AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const GOOGLE_DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";

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

/// Request data needed by a native token endpoint client.
pub struct TokenExchangeRequest {
    pub client_id: String,
    pub redirect_uri: Url,
    pub code: AuthorizationCode,
    code_verifier: SecretString,
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
        return Err(AuthError::ProviderRejected { code: code.clone() });
    }
    let code = parameters
        .get("code")
        .filter(|code| !code.is_empty())
        .ok_or(AuthError::InvalidRequest("authorization code missing"))?;
    Ok(AuthorizationCode(SecretString::from(code.clone())))
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
