# myVault — Latest Session Handoff

Updated 2026-07-14 Asia/Bangkok ค่ะ

ไฟล์นี้เป็นเจ้าของ Git checkpoint, verification ล่าสุด, งานถัดไป และ approval state ค่ะ ทิศทางผลิตภัณฑ์อยู่ที่ [PROJECT_PLAN.md](PROJECT_PLAN.md) ค่ะ

## 1. Start Here

1. รัน `git status --short --branch` และ `git diff --check` ก่อนแก้ไฟล์ค่ะ
2. อ่านหัวข้อ Current Truth, Locked Roadmap Checkpoint และ Next Actions ในไฟล์นี้ค่ะ
3. อ่าน [PROJECT_PLAN.md](PROJECT_PLAN.md) เฉพาะเมื่อต้องวาง scope หรือเปลี่ยนลำดับ milestone ค่ะ
4. อ่าน evidence เฉพาะส่วนที่เกี่ยวข้องแทนการโหลดประวัติทั้งหมดค่ะ

หาก working tree มีงานใหม่ ต้องรักษาการแก้ไขทั้งหมดและห้าม reset, checkout, clean หรือ overwrite งานเดิมค่ะ Git กับผล command ปัจจุบันเป็น source of truth เมื่อขัดกับเอกสารค่ะ

## 2. Repository Checkpoint

- Physical path คือ `/Volumes/AWB-Apps/My Apps/myVault` และ compatibility path คือ `/Users/awb/My Apps/myVault` ค่ะ
- Canonical branch คือ `main` และ canonical checkpoint คือ R1 merge commit
  `681271a` จาก PR #26 ค่ะ
- R1 live acceptance, final review, Quality, Android compile, Ubuntu AppImage,
  และ Windows NSIS ผ่านบน candidate เดียวก่อน mergeค่ะ
- Active implementation milestone คือ R2 และ shared workspace อยู่บน
  `codex/r2-guarded-transfer` ซึ่งเริ่มจาก `681271a` ค่ะ

## 3. Current Truth

- Project Complete ถูกนิยามเป็น Personal First Release ที่ผ่าน R8 ตาม [Locked Product Roadmap](PROJECT_PLAN.md) ค่ะ
- เป้าหมายระยะใกล้คือ **Safe Sync Alpha** จาก R1–R4 โดยไม่เพิ่ม knowledge features หรือ polish ระหว่างทางค่ะ
- สถานะโดยประมาณคือ 40–45% ของ personal first release เมื่อวัดจาก user-visible outcome ค่ะ
- Local Vault open/explorer/read/save และ desktop recovery snapshots เชื่อม runtime แล้วค่ะ
- Create/Rename/Move/Trash/Restore มี core/mutation foundation แต่ยังไม่มี Tauri/UI journey ครบค่ะ
- Editor/Reader ใช้งานได้บางส่วน ส่วน attachments, properties และ embeds ยังไม่ครบค่ะ
- Search/backlinks/graph ที่เห็นใน Demo เป็น filter หรือ opened-note prototype ไม่ใช่ persistent full-vault index ค่ะ
- R1 production Desktop OAuth/Keyring, Android auth bridge, exact account/root
  binding, recursive read-only scan, Changes drain, restart restoration และ
  bounded preview เชื่อม Tauri runtime แล้วค่ะ
- `myvault-sync-engine` และ production GET-only Drive adapter เป็น Tauri
  dependencies แล้ว โดย token/body/ambient path ไม่ออกสู่ frontend/SQLite/logค่ะ
- R2 implementation candidate มี guarded content upload/download, durable
  transfer state, create-no-replace local publication, exact-root Drive
  mutation boundary, desktop local observation และ Android SAF guarded runtime
  แล้วค่ะ Locked live disposable acceptance และ platform CI ยังไม่ผ่านครบค่ะ
- Conflict engine และ full Sync control-plane UI ยังเป็นงาน R3–R4 ค่ะ

## 4. Verification — R2 Candidate Audit

สถานะนี้เป็น post-audit local integration evidenceค่ะ Draft PR CI และ locked
live disposable acceptance ยังต้องผ่านก่อนเปลี่ยน PR เป็น Readyค่ะ

- `pnpm typecheck` ผ่านค่ะ
- Frontend Vitest ผ่าน 5 files / 40 tests ค่ะ
- `pnpm build` ผ่านค่ะ Main chunk ประมาณ 1.06 MB และมี non-blocking chunk-size warning ค่ะ
- `pnpm quality:r2:offline` ผ่านหลังรวม final audit fixes ทั้งหมดค่ะ
- Rust R2 matrix ครอบคลุม Core, platform ACL/FS, private FS, recovery,
  mutations, snapshots, app service, desktop auth, Drive spike, Google auth,
  private root, Vault SAF, Sync engine, Drive, transfer และ Tauriค่ะ
- `cargo fmt --manifest-path apps/tauri/src-tauri/Cargo.toml --all -- --check` ผ่านค่ะ
- Android aarch64 strict Clippy, Kotlin Vault SAF unit tests, full debug APK
  build, 16 KiB alignment, v2 signature, fresh API 36 install และ cold launch
  ผ่านค่ะ Cold launch ใช้ 669 ms และไม่พบ matching fatal process logค่ะ APK
  SHA-256 คือ
  `96d7791718cb5ba4326d74a8bd0076837f1fd52cdc8107e54a071cfbcda2c87e`ค่ะ
- Static R2 mutation/token audit ผ่าน, production dependency tree ไม่มี
  `drive-sync-spike` และ `pnpm audit --prod` ไม่พบ known vulnerabilityค่ะ

Filesystem watcher และ Unix-socket fixture ล้มเมื่อรันใน restricted sandbox แต่กรณีเดียวกันผ่านเมื่อรันด้วย native filesystem permissions ค่ะ จึงจัดเป็น environment restriction ไม่ใช่ product regression ในรอบนี้ค่ะ

Ignored-by-default tests คือ live Drive fixture และ OS keyring mutation เพราะแตะ external account/credential store ค่ะ รอบ audit นี้ไม่ได้รันสองรายการดังกล่าวค่ะ

## 5. Completed Through R2 Integration

- แยกหน้าที่เอกสารให้ `PROJECT_PLAN.md` เป็น direction/roadmap และไฟล์นี้เป็น operational handoff ค่ะ
- เปลี่ยน MVP checklist ที่กำกวมเป็น capability matrix ซึ่งแยก Usable, Prototype, Foundation only และ Missing ค่ะ
- แสดง execution order จริงว่า Phase 3 มาก่อนงาน Phase 2/4 ที่เหลือค่ะ
- ติดป้าย Phase 1 และ Phase 3A ว่า complete เฉพาะขอบเขต foundation/slice ค่ะ
- เปลี่ยน milestone ถัดไปเป็น R1 — Native Auth + Read-only Existing Drive Binding และ freeze งานที่ไม่ช่วย Safe Sync Alpha ค่ะ
- อัปเดต Sync Results จาก waiting-for-merge เป็น merged และย้าย OAuth configuration ออกจาก Phase 0 blockers ค่ะ
- ติดป้าย Demo/Phase 0 evidence เก่าให้เป็น historical หรือ pre-commit ตามจริงค่ะ
- ล็อก Personal First Release scope, Post-release scope และ execution order R1–R8 ตาม approval `Approve lock roadmap` ค่ะ
- เพิ่ม milestone dependencies, exit gates, verification matrix และ change-control rules ใน `PROJECT_PLAN.md` ค่ะ
- เพิ่ม schema v3 durable transfer queue/evidence, retry taxonomy, cursor-gated
  change batches และ restart reconciliationค่ะ
- เพิ่ม exact-root create-only Drive transfer capability, resumable upload,
  bounded blob download และ lost-response reconciliationค่ะ
- เพิ่ม private staged/base publication, guarded desktop/Android adapters,
  bounded local observation และ redacted runtime statusค่ะ

## 6. Locked Roadmap Checkpoint

- Locked sequence คือ `R1 → R2 → R3 → R4 → R5 → R6 → R7 → R8` ค่ะ
- R1–R4 ส่งมอบ Safe Sync Alpha ค่ะ
- R5 ปิด Local Product Completion ค่ะ
- R6 ปิด Persistent Knowledge Core ค่ะ
- R7 บังคับ native runtime acceptance บน macOS, Windows, Ubuntu และ physical Android ค่ะ
- R8 ทำ recovery drill, release verification และ Personal First Release ค่ะ
- Active implementation milestone คือ R2 — Guarded Upload and Download ค่ะ
- เปิด implementation milestone ได้ครั้งละหนึ่ง milestone และต้องผ่าน exit gate พร้อม approval ก่อน transition ค่ะ
- Planning range ที่เหลือจากผลรวม milestone คือ 10–19 focused engineering weeks โดยไม่รวมเวลารอ environment, device, external review หรือ account approval ค่ะ
- Scope, order และ exit gates ถูกล็อกค่ะ Planning range ไม่ใช่ deadline lock ค่ะ

## 7. Known Gaps and Direction Risks

### Product blockers

- Production guarded Drive transfer path มีแล้ว แต่ยังไม่มี locked live
  cross-device R2 evidence จาก disposable root/Vault A/Vault B ค่ะ
- ไม่มี user-visible Sync status/retry/conflict recovery ค่ะ
- Local mutation services ยังไม่ถูก expose ถึง UI ครบค่ะ

### Evidence gaps

- Windows/Ubuntu native picker persistence, Trash/Restore และ secret-store restart ยัง deferred ค่ะ
- Physical Android Play Services consent, Thai IME, lifecycle/lock-unlock และ real-GPU evidence ยัง deferred ค่ะ
- Compile, CI artifact และ emulator evidence ห้ามใช้แทน native/physical acceptance ค่ะ

### Complexity risks

- Production source มีหลาย safety-focused crates ขณะที่ Sync ยังไม่ต่อถึงแอปค่ะ ห้ามเพิ่ม abstraction ใหม่ใน 3B ถ้า reuse `desktop-auth`, `myvault-sync-engine` หรือ application service ได้ค่ะ
- Sync operational database ต้องไม่ปนกับ future content index ค่ะ
- Frontend prototype knowledge features ต้องไม่ดึง engineering effort ออกจาก Safe Sync Alpha ค่ะ

## 8. Next Actions

1. ปิด P1 จาก final audit และรัน `pnpm quality:r2:offline` จาก clean source
   candidate เดียวค่ะ
2. Rebuild/verify/install/cold-launch Android API 36 APK จาก candidate นั้นค่ะ
3. รัน locked live macOS/Android disposable round trip เมื่อ exact R2 root,
   desktop OAuth environment และ emulator test account พร้อมค่ะ
4. Push Draft PR เพื่อรัน platform CI และเก็บ exact job/artifact evidenceค่ะ
5. เปลี่ยน PR เป็น Ready และ merge เมื่อ Gate 0–8 ผ่านครบเท่านั้นค่ะ ห้ามแตะ
   personal Vault/Drive หรือเริ่ม R3 rename/move/Trash/conflict workค่ะ

## 9. Approval State

- Documentation audit, alignment, roadmap lock, PR review และ merge เข้า `main` ได้รับอนุมัติและดำเนินการแล้วค่ะ
- Locked scope/order/gates ได้รับ approval ด้วยข้อความ `Approve lock roadmap` เมื่อ 2026-07-14 และอยู่บน `main` ที่ `5160882` ค่ะ
- R1 ถูก merge ผ่าน PR #26 และ R2 transition ได้รับ approval แล้วค่ะ
- คุณโออนุมัติ R2 one-time execution ครอบคลุม code/docs/tests, subagents,
  browser OAuth, restricted full Drive re-consent, read/write เฉพาะ disposable
  R2 root, emulator, CI, commit, PR และ merge เมื่อทุก gate ผ่านค่ะ
- ไม่มี approval ด้าน User Data Policy ค้างอยู่ค่ะ OAuth credential และ token ต้องอยู่ภายนอก repository และห้ามแสดงใน log ค่ะ
- งาน implementation ใหม่ต้องเสนอแผนและขออนุมัติคุณโอก่อนลงมือค่ะ

## 10. Evidence Index

- Phase 0 feasibility และ external gates อยู่ที่ [docs/phase-0/RESULTS.md](docs/phase-0/RESULTS.md) ค่ะ
- Local Demo และ macOS UAT อยู่ที่ [docs/demo/RESULTS.md](docs/demo/RESULTS.md) ค่ะ
- Sync Foundation architecture, acceptance และผลอยู่ใน [docs/sync](docs/sync) ค่ะ
- Engineering/release history อยู่ที่ [CHANGELOG.md](CHANGELOG.md) ค่ะ
