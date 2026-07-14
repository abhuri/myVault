# myVault

`myVault` is a local-first, cross-platform Markdown vault application with direct Google Drive synchronizationค่ะ

The first release targets macOS, Windows, Ubuntu, and Android through Tauri 2 with a React and TypeScript UI and a Rust native coreค่ะ

## Status

- `v0.1.0-demo` เปิด Local Vault, แสดง explorer, อ่าน/แก้ Markdown ด้วย revision-checked autosave และมี Reader, Mermaid, outline, quick switcher, opened-note backlinks กับ graph prototype ค่ะ
- Phase 1 local safety foundation และ macOS live Copy-of-Vault UAT ผ่านแล้วค่ะ Windows/Ubuntu native runtime และ physical Android evidence ยัง deferred ค่ะ
- Phase 3A Sync Foundation ถูก merge ผ่าน PR #23 ที่ `db85177` แล้วค่ะ
- R1 Native Auth + Read-only Existing Drive Binding ถูก merge ผ่าน PR #26 ที่
  `681271a` แล้วค่ะ Production OAuth, exact account/root binding, read-only
  scan, Changes drain และ redacted Tauri status เชื่อม runtime แล้วค่ะ
- R2 Guarded Transfer อยู่ในสถานะ implementation candidate บน
  `codex/r2-guarded-transfer` ค่ะ Upload/download แบบ byte-verified,
  create-no-replace, durable retry/reconciliation และ Android SAF runtime ถูก
  implement แล้ว และ quality/Android/Ubuntu/Windows CI ผ่านบน candidate แล้วค่ะ
  Locked live disposable round trip ยังต้องผ่านก่อนประกาศ complete หรือ mergeค่ะ
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

Run the current R2 offline verification contractค่ะ

```bash
pnpm quality:r2:offline
pnpm tauri:info
```

Synthetic Vault สำหรับทดลองอยู่ที่ `demo/synthetic-vault` ค่ะ ควร copy ไป temporary folder ก่อนทดสอบ save/conflict เพื่อไม่แก้ fixture ใน repository ค่ะ

Historical Phase 0 platform gates and environment gaps are documented in
[docs/phase-0](docs/phase-0)ค่ะ Current R2 scope, bounds, disposable-root policy,
and exit gates are defined by [docs/sync/R2_PLAN.md](docs/sync/R2_PLAN.md) and
[docs/sync/R2_ACCEPTANCE.md](docs/sync/R2_ACCEPTANCE.md)ค่ะ Live tests are opt-in
and may touch only the exact recorded disposable R2 account/root and disposable
local Vaultsค่ะ Physical Android acceptance remains deferred to R7ค่ะ
