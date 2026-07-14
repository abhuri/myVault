# myVault

`myVault` is a local-first, cross-platform Markdown vault application with direct Google Drive synchronizationค่ะ

The first release targets macOS, Windows, Ubuntu, and Android through Tauri 2 with a React and TypeScript UI and a Rust native coreค่ะ

## Status

- `v0.1.0-demo` เปิด Local Vault, แสดง explorer, อ่าน/แก้ Markdown ด้วย revision-checked autosave และมี Reader, Mermaid, outline, quick switcher, opened-note backlinks กับ graph prototype ค่ะ
- Phase 1 local safety foundation และ macOS live Copy-of-Vault UAT ผ่านแล้วค่ะ Windows/Ubuntu native runtime และ physical Android evidence ยัง deferred ค่ะ
- Phase 3A Sync Foundation ถูก merge ผ่าน PR #23 ที่ `db85177` แล้วค่ะ Foundation นี้ยังไม่ถูกต่อเข้ากับ Tauri/UI และยังไม่มี production Drive read/write ค่ะ
- เป้าหมายถัดไปคือ Phase 3B Native Auth + Read-only Existing Drive Binding ซึ่งยังต้องขอ approval ก่อนแตะ OAuth runtime หรือ Google Drive จริงค่ะ
- Roadmap ถูกล็อกเป็น `R1 → R2 → R3 → R4 → R5 → R6 → R7 → R8` จนถึง Personal First Release โดยรายละเอียด scope และ exit gates อยู่ใน [PROJECT_PLAN.md](PROJECT_PLAN.md) ค่ะ

สถานะโดยประมาณคือ 40–45% ของ personal first release เมื่อวัดจาก user-visible outcome ค่ะ รายละเอียดทิศทางและ capability gaps อยู่ใน [PROJECT_PLAN.md](PROJECT_PLAN.md) ส่วน Git checkpoint, verification ล่าสุด และงานถัดไปอยู่ใน [SESSION_HANDOFF.md](SESSION_HANDOFF.md) ค่ะ

ผล Demo อยู่ที่ [docs/demo/RESULTS.md](docs/demo/RESULTS.md) และหลักฐาน Sync Foundation อยู่ที่ [docs/sync/RESULTS.md](docs/sync/RESULTS.md) ค่ะ

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
