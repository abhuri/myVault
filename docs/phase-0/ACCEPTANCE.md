# Phase 0 Technical Spike Acceptance Contract

Phase 0 exists to prove the risky platform boundaries before product development continuesค่ะ

## Gate 1 — Android Google Authorization

- Android uses a Kotlin Tauri mobile plugin around Google Identity Services `AuthorizationClient` ค่ะ
- Android must not use a loopback redirect or custom URI scheme for Google OAuth ค่ะ
- A Drive-capable access token remains in the native layer and is never exposed to Reactค่ะ
- Consent, cancellation, expiry, foreground resume, revoke, missing Play Services, and activity recreation have explicit outcomesค่ะ
- Failure must never damage local Vault data or advance the Drive changes cursorค่ะ

## Gate 2 — Android WebView and Thai IME

- CodeMirror handles Thai composition, autocorrect, selection, undo, redo, and clipboard on a physical Android deviceค่ะ
- Virtual keyboard resizing, safe areas, orientation changes, suspend, resume, and process recreation are testedค่ะ
- The actual Android System WebView version is recordedค่ะ
- Target p95 input-to-paint latency is below 50 milliseconds for the standard fixture noteค่ะ
- Mermaid renders safely and a 1,000-node Sigma graph remains interactiveค่ะ
- A 5,000-node graph is measured as capacity data but is not a mandatory go gateค่ะ

## Gate 3 — Desktop Filesystem Safety

- macOS, Windows, and Ubuntu can select and persist a local Vault folder across restartค่ะ
- Read, atomic write, rename, move, trash, and restore work with Thai, Unicode, spaces, and long filenamesค่ะ
- A temp-write, flush, sync, and rename sequence survives interruption without truncating the source fileค่ะ
- The watcher normalizes create, modify, rename, delete, event bursts, and self-write suppressionค่ะ
- Symlinks escaping the Vault root are rejected by defaultค่ะ
- `.obsidian` is preserved and denied for automatic writesค่ะ

## Gate 4 — SQLite and Secret Storage

- SQLite migrations, transactions, rollback, abrupt process termination, reopen, and forward migration pass on all targetsค่ะ
- The database can be deleted and rebuilt from the fixture Vaultค่ะ
- Tokens never persist in JavaScript storage, SQLite, logs, environment dumps, or plaintext filesค่ะ
- Desktop secure storage and Android Keystore integration survive restart and remove credentials on logoutค่ะ
- Stronghold may be evaluated as an encrypted vault but must not be treated as an OS keystore by itselfค่ะ

## Gate 5 — Drive Round Trip

- A fixture Markdown file and binary attachment round-trip desktop to Drive to Android with byte/hash verificationค่ะ
- Initial sync takes a changes start token before scanning and drains changes after the scanค่ะ
- Pagination, rename, move, trash, restore, offline retry, `401`, `403`, `429`, and `5xx` behavior are testedค่ะ
- The changes cursor advances only after every associated local mutation commits successfullyค่ะ
- Unknown upload outcomes are verified remotely before retry to prevent duplicate filesค่ะ
- No operation may touch data outside the allowlisted fixture folderค่ะ

## Cross-platform Build Gate

- The same committed React UI builds on macOS, Windows, Ubuntu, and Androidค่ะ
- Each platform records Node, pnpm, Rust, Tauri, WebView, OS, SDK, and architecture versionsค่ะ
- Desktop artifacts are built natively on their corresponding operating systemsค่ะ
- Ubuntu AppImage uses an Ubuntu 22.04-compatible build baselineค่ะ
- Android uses an NDK version compatible with 16 KB page-size requirementsค่ะ

## Fixture Safety

The Drive fixture folder must be named `myVault-spike-<date>-<random>` and cleanup must verify the exact folder ID and prefix before moving it to Trashค่ะ

The fixture must contain the followingค่ะ

- `hello.md` ค่ะ
- `thai-สวัสดี.md` ค่ะ
- three nested directory levelsค่ะ
- duplicate filenames in different folders and, where Drive permits, the same folderค่ะ
- an empty fileค่ะ
- a small binary fileค่ะ
- an attachment larger than 5 MB for resumable uploadค่ะ
- spaces, Unicode, and apostrophes in pathsค่ะ
- `.obsidian/ignored.json` as a preservation probeค่ะ

Permanent deletion is forbidden during Phase 0ค่ะ

