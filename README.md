# myVault

`myVault` is a local-first, cross-platform Markdown vault application with direct Google Drive synchronizationค่ะ

The first release targets macOS, Windows, Ubuntu, and Android through Tauri 2 with a React and TypeScript UI and a Rust native coreค่ะ

## Status

`v0.1.0-demo` เป็น Local Desktop Demo สำหรับใช้คนเดียวบน macOS ก่อนค่ะ รุ่นนี้เปิด Vault ผ่าน native folder picker, แสดง file explorer, แก้ Markdown แบบ revision-checked autosave, อ่าน GFM/table/code/Mermaid และมี outline, backlinks, quick switcher กับ graph prototype ค่ะ

Google Drive Sync, physical Android acceptance และ store distribution ยังไม่รวมใน Demo ค่ะ ผลทดสอบล่าสุดอยู่ที่ [docs/demo/RESULTS.md](docs/demo/RESULTS.md) และเกณฑ์ตรวจอยู่ที่ [docs/demo/ACCEPTANCE.md](docs/demo/ACCEPTANCE.md) ค่ะ

See [PROJECT_PLAN.md](PROJECT_PLAN.md) for architecture decisions, delivery phases, safety rules, and the current session handoffค่ะ

## First-release constraints

- Personal use onlyค่ะ
- No application backendค่ะ
- No hosting or VPNค่ะ
- No App Store or Play Store distributionค่ะ
- No real vault data, OAuth tokens, signing keys, or credentials may be committedค่ะ

## Development

Install dependencies and run the native Demo ค่ะ

```bash
pnpm install --frozen-lockfile
pnpm --dir apps/tauri tauri dev
```

Run the baseline verification contractค่ะ

```bash
pnpm typecheck
pnpm test
pnpm build
cargo fmt --manifest-path apps/tauri/src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path apps/tauri/src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path apps/tauri/src-tauri/Cargo.toml
pnpm test:rust
pnpm tauri:info
```

Synthetic Vault สำหรับทดลองอยู่ที่ `demo/synthetic-vault` ค่ะ ควร copy ไป temporary folder ก่อนทดสอบ save/conflict เพื่อไม่แก้ fixture ใน repository ค่ะ

Phase 0 platform gates and environment gaps are documented in [docs/phase-0](docs/phase-0)ค่ะ

The Google Drive live harness and physical-device tests are opt-in and may touch only a verified `myVault-spike-<date>-<random>` fixture folderค่ะ Follow [docs/phase-0/DEVICE_TEST.md](docs/phase-0/DEVICE_TEST.md) before enabling themค่ะ
