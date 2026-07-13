# Phase 0 Physical Android and Google OAuth Runbook

This runbook is the remaining manual evidence path for Phase 0ค่ะ

Current status as of 2026-07-13: automated Phase 0 CI is complete at commit `0aecda5`ค่ะ คุณโออนุมัติให้ Sunday perform the Google Cloud configuration through the logged-in browser sessionค่ะ Android testing uses the API 36 emulator until a spare physical device becomes available; final `.apk` sideload and physical-device checks remain deferred and do not block Phase 1ค่ะ

Storage policy: install additional Android development tools, SDK components, system images, AVDs, and large caches on `AWB-Apps` or `AWB Storage` whenever supportedค่ะ Keep Macintosh HD for components that genuinely require the internal diskค่ะ

## Google Cloud Configuration

Owner: Sundayค่ะ Creating/selecting the project and OAuth clients is an approved external actionค่ะ Do not change billing, publish the OAuth app, or add users other than the personal test account without new approvalค่ะ

Progress: completed on 2026-07-13ค่ะ Project `myVault Personal` (`myvault-personal-0aecda5`) exists, Drive API is enabled, the User Data Policy is accepted, the app is External/Testing with only the approved personal test user, the restricted full Drive scope is saved, and Android plus Desktop OAuth clients are createdค่ะ The live Desktop OAuth + PKCE fixture harness passed and verified trash-only cleanupค่ะ

1. Create or select a Google Cloud project owned by the personal account that will use myVaultค่ะ
2. Enable Google Drive APIค่ะ
3. Configure the OAuth consent screen for external/personal testing and add the personal Google account as a test user when requiredค่ะ
4. Create an Android OAuth client with package name `com.abhuri.myvault`ค่ะ
5. Register debug certificate SHA-1 `B7:5C:6B:B2:47:B8:E6:78:95:0E:D2:E4:DE:69:7B:7E:25:D5:59:70`ค่ะ
6. Create a Desktop OAuth client for the loopback + PKCE spikeค่ะ
7. Never commit downloaded client-secret JSON, access tokens, refresh tokens, signing keys, or screenshots containing consent-account detailsค่ะ

The full Drive scope is restricted and may display an unverified-app warning for a personal test applicationค่ะ Do not publish the OAuth app or add other users during Phase 0ค่ะ

## Connect the Physical Device

Status: deferred by user because no spare physical Android device is currently availableค่ะ Continue emulator coverage in the meantime, then resume this section for final `.apk` sideload evidence when a device with Google Play Services can be connectedค่ะ

Emulator status: production Android Vault activation through the Storage Access Framework, persisted permission, inventory, Thai/Unicode read, guarded save, Reader scrolling, and Mermaid rendering passed on API 36 on 2026-07-13ค่ะ SAF guarded save uses a synchronized revision compare, descriptor sync, and byte-for-byte readback verificationค่ะ It does not claim the desktop core's descriptor-relative atomic rename plus parent-directory fsync contract; unsupported directory durability is reported as `directorySyncUnsupported`, and uncertain publication is reported as `writeOutcomeUnknown`ค่ะ The final uncommitted checkpoint passes 3 SAF policy tests, Android aarch64 Clippy, full debug APK build, 16 KB zip alignment, and APK Signature Scheme v2; APK SHA-256 is `ace5ca1504ea06a0964a67904172b21d1babc2630b999e3ea18b9a803fd20a5f`ค่ะ Physical-device validation remains necessary for real IME behavior, GPU rendering, lock/unlock, and final sideload acceptanceค่ะ

1. Enable Developer options and USB debugging on the Android deviceค่ะ
2. Connect USB and approve the computer fingerprint on the deviceค่ะ
3. Confirm the device with `adb devices -l`ค่ะ
4. Install the current APK with `adb install -r apps/tauri/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk`ค่ะ
5. Record the actual WebView with `adb shell dumpsys package com.google.android.webview | rg 'versionName='`ค่ะ

## Required Test Matrix

- Type Thai with composition, autocorrect, selection handles, undo, redo, copy, paste, and multiline inputค่ะ
- Record at least 30 composition samples and confirm p95 composition-to-paint is below 50 ms on the standard fixture noteค่ะ
- Open and close the keyboard, rotate portrait/landscape, background/resume, lock/unlock, and force-stop/relaunchค่ะ
- Confirm Mermaid remains sanitized and visibleค่ะ
- Confirm the 1,000-node Sigma graph pans and zooms interactivelyค่ะ
- Record the 5,000-node graph as capacity data; failure does not block Phase 0ค่ะ
- Run Google connect and test consent success, user cancellation, repeated connect, cold-process reconnect, disconnect, and reconnect after revokeค่ะ
- Disconnect network during a local edit and confirm no Vault file is damagedค่ะ
- Capture logcat and confirm no bearer token, authorization code, refresh token, fatal exception, or ANR appearsค่ะ

## Drive Fixture Rules

- The folder must be named `myVault-spike-<date>-<random>`ค่ะ
- Record its exact Drive folder ID before any cleanupค่ะ
- Cleanup may move only that verified folder ID to Trashค่ะ
- Permanent deletion is forbiddenค่ะ
- No operation may touch an existing personal Vault during Phase 0ค่ะ
