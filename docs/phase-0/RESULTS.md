# Phase 0 Evidence and Gate Status

Updated 2026-07-12 Asia/Bangkokค่ะ

Status values are `PASS`, `PARTIAL`, and `BLOCKED`ค่ะ A gate is never marked `PASS` from compilation or mocks when its contract requires a physical device, another native operating system, or a live Google Drive accountค่ะ

## Gate Summary

| Gate | Status | Executable evidence | Remaining evidence |
|---|---|---|---|
| Android Google Authorization | PARTIAL | Kotlin Tauri plugin compiles in the ARM64 APK; GIS `AuthorizationClient`, cancellation paths, duplicate-operation guard, scope verification, revoke/clear, and native-only zeroizing token boundary are implementedค่ะ | Google Cloud Android OAuth client and a real device with Play Services are required for consent, cancel, cold-start reacquisition, and revoke testsค่ะ |
| Android WebView and Thai IME | PARTIAL | API 36 ARM64 emulator launches; CodeMirror and Mermaid render; lifecycle cold restart and rotation produce no crash; WebView 133 is recorded; the 1,000-node Sigma graph renders after visibility refreshค่ะ | Thai composition, autocorrect, clipboard, keyboard resize, and p95 input-to-paint require a physical Android deviceค่ะ The 5,000-node capacity probe blacked out the headless emulator surface and is recorded as non-gating capacity dataค่ะ |
| Desktop Filesystem Safety | PARTIAL | `myvault-core` uses descriptor-relative no-follow traversal and atomic temp/fsync/rename against an already-open parent; adversarial symlink-swap, Thai/Unicode/spaces, `.obsidian`, watcher burst, and self-write tests passค่ะ | Native folder-picker persistence and trash/restore remain to run on macOS, Windows, and Ubuntu shellsค่ะ |
| SQLite and Secret Storage | PARTIAL | SQLite migrations, transactions, rollback, rebuild, and Unicode round trips pass; desktop OAuth passes seven isolated tests; an ephemeral macOS Keychain save/load/delete round trip passes; secrets redact from `Debug`ค่ะ | Windows Credential Manager and Linux Secret Service restart tests remainค่ะ Android access tokens intentionally remain memory-only and are reacquired through GIS rather than persisted in app storageค่ะ |
| Drive Round Trip | PARTIAL | The complete acceptance fixture, pagination, start-token ordering, exact mutation-ID cursor commits, >5 MiB resumable upload, byte/hash verification, unknown outcomes, random cleanup marker, same-origin bearer policy, and verified trash-only cleanup pass mock testsค่ะ | The same harness must run against live Drive after Google OAuth configurationค่ะ |
| Cross-platform Build | PASS | At commit `0aecda5`, GitHub quality, Android compile + 16 KB alignment, Windows Server 2022 NSIS, and Ubuntu 22.04 AppImage jobs are greenค่ะ macOS native debug build and API 36 emulator launch also pass locallyค่ะ | Store signing and public distribution remain intentionally outside Phase 0ค่ะ |

## Automated Test Evidence

- Frontend Vitest: 8 tests passค่ะ
- Tauri application Rust: 2 tests passค่ะ
- Filesystem, watcher, and SQLite core: 13 tests passค่ะ
- Desktop OAuth and secure store abstraction: 9 tests passค่ะ
- macOS Keychain live adapter: 1 ignored-by-default ephemeral save/load/delete test passes when explicitly enabledค่ะ
- Drive state machine, REST, fixture, resumable-upload, and adversarial safety suite: 25 tests pass with 1 live test ignored by defaultค่ะ
- Android Kotlin/GIS bridge: ARM64 debug APK compiles successfullyค่ะ
- APK 16 KB alignment: Build Tools 36 `zipalign -c -P 16 -v 4` reports `Verification successful`ค่ะ
- GitHub CI at commit `0aecda5`: quality, Android compile + 16 KB alignment, Windows NSIS, and Ubuntu 22.04 AppImage jobs passค่ะ

## Recorded Environment

- Host: macOS 26.5.2 arm64ค่ะ
- Node: 24.14.1ค่ะ
- pnpm: 11.7.0ค่ะ
- Rust: 1.96.0ค่ะ
- Android emulator: Android 16 / API 36 / arm64-v8aค่ะ
- Android System WebView: 133.0.6943.137ค่ะ
- Android SDK: API 36 with Build Tools 36.0.0ค่ะ
- Android NDK: 29.0.13846066ค่ะ
- APK SHA-256: `dfa259d379b9cb20163185b32b8c721a7fcf8f92ad42f3fb5fc0381e4d7bef47`ค่ะ

## Security Audit Closure

- Filesystem writes no longer use ambient check-then-open paths; a parent-directory symlink swap cannot redirect the atomic commitค่ะ
- Drive cleanup no longer trusts caller-provided identity; remote metadata and a random 256-bit marker must match before Trashค่ะ
- Bearer tokens are restricted to exact Google HTTPS origins and resumable session URLs must remain same-originค่ะ
- Desktop and Android native boundaries allow only the required full Drive scopeค่ะ
- Android authorization has a 120-second timeout and clears local session state after token clear even when best-effort revoke failsค่ะ
- Gradle wrapper distribution SHA-256 and dependency verification metadata are committed, and CI compiles the Kotlin GIS bridge into an Android APKค่ะ

## Emulator Findings

- Cold launch completed in approximately 2.0 seconds on the first clean emulator launch and approximately 0.7 seconds on later cold launchesค่ะ
- Home/resume, forced process stop/relaunch, and rotation completed without a fatal exception or ANR in captured logcat outputค่ะ
- Mermaid strict rendering is visible in the captured evidenceค่ะ
- The 1,000-node Sigma graph became visible after adding an intersection-triggered resize and refresh, with a recorded first-paint measurement of approximately 358.5 ms on the headless software-rendered emulatorค่ะ
- Selecting the 5,000-node capacity probe caused the headless emulator display surface to become black without a Java crashค่ะ This is not a mandatory gate but must be repeated on a physical GPU before capacity decisionsค่ะ

## Evidence Files

- `evidence/android-emulator-api36.png` — Android native bridge and CodeMirror/Thai fixtureค่ะ
- `evidence/android-emulator-graph-refresh.png` — Mermaid, 1,000-node Sigma graph, WebView user agent, and first-paint dataค่ะ
- `evidence/android-emulator-graph-5000.png` — black-surface result from the non-gating 5,000-node emulator capacity attemptค่ะ

## External Validation Status

Phase 0 automated gates are complete at commit `0aecda5`ค่ะ The remaining external evidence does not block starting Phase 1ค่ะ

1. Google Cloud project `myVault Personal` (`myvault-personal-0aecda5`) exists and Drive API is enabledค่ะ OAuth branding is staged but policy acceptance and Android/Desktop OAuth client creation remain pendingค่ะ
2. Physical Android validation for Play Services consent, Thai IME, lifecycle, and real-GPU graph behavior is deferred because no device is currently availableค่ะ

No real Drive folder has been created and no user data has been touchedค่ะ

## Product Decisions Entering Phase 1

- Deletion uses Vault-local `.trash/` for consistent atomic move and restore behavior across platformsค่ะ
- `.trash/` is excluded from the normal explorer, index, search, backlinks, and graphค่ะ
- Rename, move, restore, and create operations must never silently overwrite an existing destinationค่ะ
- Recovery snapshots default to 30 days, at most 100 revisions per note, and at most 1 GiB per Vaultค่ะ
