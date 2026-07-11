# Phase 0 Physical Android and Google OAuth Runbook

This runbook is the remaining manual evidence path for Phase 0ค่ะ

## Google Cloud Configuration

1. Create or select a Google Cloud project owned by the personal account that will use myVaultค่ะ
2. Enable Google Drive APIค่ะ
3. Configure the OAuth consent screen for external/personal testing and add the personal Google account as a test user when requiredค่ะ
4. Create an Android OAuth client with package name `com.abhuri.myvault`ค่ะ
5. Register debug certificate SHA-1 `B7:5C:6B:B2:47:B8:E6:78:95:0E:D2:E4:DE:69:7B:7E:25:D5:59:70`ค่ะ
6. Create a Desktop OAuth client for the loopback + PKCE spikeค่ะ
7. Never commit downloaded client-secret JSON, access tokens, refresh tokens, signing keys, or screenshots containing consent-account detailsค่ะ

The full Drive scope is restricted and may display an unverified-app warning for a personal test applicationค่ะ Do not publish the OAuth app or add other users during Phase 0ค่ะ

## Connect the Physical Device

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
