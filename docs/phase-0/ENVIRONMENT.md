# Phase 0 Environment Record

Recorded on 2026-07-11 in Asia/Bangkokค่ะ

## Current macOS host

| Tool | Observed state |
|---|---|
| Git | `2.50.1` ค่ะ |
| GitHub CLI | `2.94.0` and authenticatedค่ะ |
| Node.js | `24.14.1` ค่ะ |
| npm | `11.11.0` ค่ะ |
| pnpm | `11.7.0` ค่ะ |
| Rust | `1.96.0` stable on `aarch64-apple-darwin` ค่ะ |
| Cargo | `1.96.0` ค่ะ |
| Xcode | Full Xcode not installedค่ะ |
| Xcode Command Line Tools | Installedค่ะ |
| Java | Not installed or not available in `PATH` ค่ะ |
| Android SDK/NDK | Not installed or not detectedค่ะ |

## Blocking prerequisites

- Install full Xcode before the complete macOS packaging checkค่ะ
- Install Android Studio or a compatible JDK, Android SDK Platform/Tools, Build Tools, Command-line Tools, and side-by-side NDKค่ะ
- Set `JAVA_HOME`, `ANDROID_HOME`, and `NDK_HOME`ค่ะ
- Add the Rust Android targets required by Tauriค่ะ
- Prefer an NDK version meeting current Android 16 KB page-size guidanceค่ะ

Environment installation is intentionally separate from source bootstrap so missing tools remain visible rather than being silently assumedค่ะ
