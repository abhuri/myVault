# myVault desktop authentication spike

This isolated crate proves the desktop authentication boundary without needing
Google client credentials or touching the Tauri command surface.

It provides:

- Google installed-app authorization URLs with PKCE S256 and a random state.
- A loopback callback listener bound only to literal `127.0.0.1` on an
  operating-system-selected port.
- Strict callback path/state checks and a bounded wait.
- A token exchange trait that can be mocked without a network connection.
- Refresh-token storage through the native OS credential store.

The UI should receive only the authorization URL and high-level status. It must
never receive an access token, refresh token, PKCE verifier, or OAuth callback
payload.

## Integration outline

1. Construct `DesktopOAuth::bind` in the Rust/Tauri layer.
2. Open `authorization_url()` with the platform browser.
3. Call `wait_for_callback` on a worker thread with a short timeout.
4. Pass `TokenExchangeRequest` to the native HTTP implementation of
   `TokenExchanger`.
5. Save only a returned refresh token with `SecretStore::save_refresh_token`.
6. On logout, revoke remotely when possible, then call
   `SecretStore::delete_refresh_token` even if revocation fails.

On Linux, the default keyring backend uses the freedesktop Secret Service over
D-Bus. A desktop session must have an unlocked provider such as GNOME Keyring
or KWallet. Headless/minimal sessions may have no provider and should surface a
clear "secure storage unavailable" error rather than falling back to plaintext.
