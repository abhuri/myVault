# Phase 0 OAuth and Google Drive Contract

## Boundary

The React layer may request authorization status and high-level Drive operations but must never receive access tokens, refresh tokens, authorization codes, PKCE verifiers, or authorization headersค่ะ

The shared interface is conceptually defined as followsค่ะ

```text
GoogleAuthorizationProvider
├── authorize_drive()
├── get_fresh_access_token_for_native_drive_client()
├── revoke()
└── status()
```

Drive calls run in Rust or a platform-native plugin behind narrow typed Tauri commandsค่ะ

## Desktop Authorization

- Use a Google Desktop OAuth clientค่ะ
- Open the system browser and never an embedded WebViewค่ะ
- Bind a one-shot listener to literal `127.0.0.1` on a random available portค่ะ
- Use Authorization Code with PKCE S256 and a cryptographically random `state` valueค่ะ
- Reject state mismatch, repeated callbacks, expired callbacks, and callback paths not owned by the flowค่ะ
- Use the exact redirect URI for both authorization and token exchangeค่ะ
- Treat the native client as a public client and do not depend on an embedded client secretค่ะ
- Keep access tokens in memory and refresh tokens only in approved OS secure storageค่ะ

## Android Authorization

- Use a Kotlin Tauri plugin wrapping Google Identity Services `AuthorizationClient`ค่ะ
- Associate the Android OAuth client with the package name and signing-certificate SHA-1ค่ะ
- Use separate OAuth clients when debug and release signing certificates differค่ะ
- Do not use desktop loopback behavior on Androidค่ะ
- Do not use a custom scheme/AppAuth flow as the Google baselineค่ะ
- Treat background authorization as platform-governed and require foreground/manual recoveryค่ะ

## Drive Scope

The Existing Vault spike uses `https://www.googleapis.com/auth/drive` because the app must see files created by Obsidian and other toolsค่ะ

This is a restricted scope and is acceptable for the personal-use spike with an unverified-app warningค่ะ

The narrower `drive.file` scope may be evaluated separately but must not be assumed to cover arbitrary existing files or future files created by other applicationsค่ะ

## Initial Scan Without Lost Changes

1. Request `changes.getStartPageToken` before the full scanค่ะ
2. Recursively scan the allowlisted fixture folder with paginationค่ะ
3. Persist file IDs, ancestry, metadata, and content hashes locallyค่ะ
4. Drain `changes.list` starting from the token captured before the scanค่ะ
5. Apply all returned current states and continue through every `nextPageToken`ค่ะ
6. Atomically persist `newStartPageToken` only after local application succeedsค่ะ

Changes entries represent current state rather than a deltaค่ะ Removed items may no longer contain parent information, so ancestry cannot be inferred only from the latest remote parent fieldค่ะ

## Retry Rules

- `401` triggers one serialized refresh or reauthorization attemptค่ะ
- `403`, `429`, and transient `5xx` responses use exponential backoff with jitterค่ะ
- Retry honors server guidance such as `Retry-After` when availableค่ะ
- An indeterminate upload outcome is verified through remote metadata and hashes before creating or uploading againค่ะ
- Logs redact tokens, authorization codes, verifiers, note bodies, and attachment contentsค่ะ

## Google Cloud Console Setup

- Create or select the project used only for myVault developmentค่ะ
- Enable Google Drive APIค่ะ
- Configure Branding, Audience, and Data Accessค่ะ
- Use an External audience and add the personal Google account as a test user during the spikeค่ะ
- Add the restricted Drive scopeค่ะ
- Create one Desktop OAuth client for macOS, Windows, and Ubuntu spike buildsค่ะ
- Create Android OAuth clients for the debug and release package/signing combinationsค่ะ
- Keep exported credential files and all tokens outside Gitค่ะ

## Live Execution Evidence — 2026-07-13

- Google Auth Platform is configured as External/Testing with only the approved personal test accountค่ะ
- The exact full Drive scope is saved, and separate Android plus Desktop OAuth clients are activeค่ะ
- Desktop loopback OAuth with PKCE completed successfullyค่ะ Credential JSON and short-lived tokens remained outside the repositoryค่ะ
- The env-gated acceptance harness created `myVault-spike-2026-07-13-31b8e077`, exercised the nested Unicode fixtures and resumable upload, listed the fixture, and moved only verified folder ID `1Ob-tJFCpYQ4KxXAMscMf_5isRYsDXWMZ` to Trashค่ะ
- A post-cleanup exact-name query returned one matching folder with `trashed=true` and the expected random fixture markerค่ะ No permanent delete was usedค่ะ
- This closes the Phase 0 Drive round-trip spike onlyค่ะ Production Drive Sync remains a separate Phase 3 milestoneค่ะ
