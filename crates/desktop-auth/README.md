# myVault desktop authentication spike

This isolated crate proves the desktop authentication boundary without using
live Google credentials in tests or touching the Tauri command surface.

It provides:

- Google installed-app authorization URLs with PKCE S256 and a random state.
- A loopback callback listener bound only to literal `127.0.0.1` on an
  operating-system-selected port.
- Strict callback path/state checks and a bounded wait.
- A Google token-endpoint client for authorization-code exchange and refresh,
  with redirects disabled, bounded responses, a native-only client credential,
  and redacted errors.
- Mockable token exchange/refresh traits for offline tests.
- Refresh-token storage through the native OS credential store.
- A native provider that rotates stored refresh tokens and returns fresh access
  tokens without making them serializable.

The native runtime opens the authorization URL in the system browser. The UI
receives only high-level status and must never receive an authorization URL,
access token, refresh token, PKCE verifier, or OAuth callback payload.

## Integration outline

1. Load the Desktop OAuth client ID and client secret from the native runtime
   environment. Never commit either value or serialize the secret to the UI.
2. Parse the secret into `GoogleClientSecret`, then construct
   `GoogleTokenClient` in the Rust/Tauri layer.
3. Construct `DesktopOAuth::bind` in the Rust/Tauri layer.
4. Open `authorization_url()` from native code with the system browser.
5. Call `wait_for_callback` on a worker thread with a short timeout.
6. Pass `TokenExchangeRequest` to `GoogleTokenClient` through the native
   `TokenExchanger` boundary.
7. Discover the provider-stable account ID using the returned access token,
   then save only the refresh token with `NativeTokenProvider`.
8. Request later access tokens through `NativeTokenProvider::fresh_access_token`.
9. On logout, revoke remotely when possible, then call the idempotent
   `NativeTokenProvider::disconnect` even if revocation fails.

On Linux, the default keyring backend uses the freedesktop Secret Service over
D-Bus. A desktop session must have an unlocked provider such as GNOME Keyring
or KWallet. Headless/minimal sessions may have no provider and should surface a
clear "secure storage unavailable" error rather than falling back to plaintext.
