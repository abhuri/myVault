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
- Canonical branch คือ `main` และ canonical checkpoint คือ merge commit `5160882` จาก PR #24 ค่ะ
- Phase 3A source head `7f5b8d6` ถูก merge ผ่าน PR #23 ที่ `db85177` และ locked roadmap commit `f5fba4d` ถูก merge ผ่าน PR #24 ที่ `5160882` ค่ะ
- PR #24 ผ่าน Final Merge Review, Quality, Android compile, Ubuntu AppImage และ Windows NSIS ก่อน merge ค่ะ
- Post-merge Quality run `29295471872` บน `5160882` ผ่านทั้ง `quality` และ `android-compile` ค่ะ
- Active implementation milestone คือ R1 ค่ะ Shared workspace ล่าสุดอยู่บน `codex/r1-readonly-binding` ซึ่งแตกจาก `5160882` และมีงานที่ต้องรักษาไว้ค่ะ Session ใหม่ต้องตรวจ Git state จริงเพราะ active branch อาจเดินหน้าจาก checkpoint นี้แล้วค่ะ

## 3. Current Truth

- Project Complete ถูกนิยามเป็น Personal First Release ที่ผ่าน R8 ตาม [Locked Product Roadmap](PROJECT_PLAN.md) ค่ะ
- เป้าหมายระยะใกล้คือ **Safe Sync Alpha** จาก R1–R4 โดยไม่เพิ่ม knowledge features หรือ polish ระหว่างทางค่ะ
- สถานะโดยประมาณคือ 40–45% ของ personal first release เมื่อวัดจาก user-visible outcome ค่ะ
- Local Vault open/explorer/read/save และ desktop recovery snapshots เชื่อม runtime แล้วค่ะ
- Create/Rename/Move/Trash/Restore มี core/mutation foundation แต่ยังไม่มี Tauri/UI journey ครบค่ะ
- Editor/Reader ใช้งานได้บางส่วน ส่วน attachments, properties และ embeds ยังไม่ครบค่ะ
- Search/backlinks/graph ที่เห็นใน Demo เป็น filter หรือ opened-note prototype ไม่ใช่ persistent full-vault index ค่ะ
- Desktop OAuth/Keyring primitives, Android auth bridge และ Drive fixture harness มีจริง แต่ยังไม่รวมเป็น production authorization/Drive runtime เดียวกันค่ะ
- Phase 3A Sync Foundation complete ตาม slice และผ่าน 17 tests แต่ `myvault-sync-engine` ยังไม่เป็น dependency ของ Tauri app ค่ะ
- แอปยังไม่มี production Existing Drive binding, read-only scan, upload/download, conflict engine หรือ Sync UI ค่ะ
- UI ยังแสดงข้อความตามจริงว่า Demo ไม่เชื่อม Google Drive ค่ะ

## 4. Verification — Current Audit

รันเมื่อ 2026-07-14 บน macOS workspace ปัจจุบันค่ะ

- `pnpm typecheck` ผ่านค่ะ
- Frontend Vitest ผ่าน 4 files / 24 tests ค่ะ
- `pnpm build` ผ่านค่ะ Main chunk ประมาณ 1.06 MB และมี non-blocking chunk-size warning ค่ะ
- Rust native test matrix ผ่าน 399 tests, 0 failed และ 2 ignored-by-default tests ค่ะ Matrix ครอบคลุม Tauri, Core, platform ACL/FS, private FS, recovery, mutations, snapshots, app service, desktop auth, Drive spike และ Sync Foundation ค่ะ
- `cargo fmt --manifest-path apps/tauri/src-tauri/Cargo.toml --all -- --check` ผ่านค่ะ
- `git diff --check` ผ่านหลัง documentation alignment ค่ะ

Filesystem watcher และ Unix-socket fixture ล้มเมื่อรันใน restricted sandbox แต่กรณีเดียวกันผ่านเมื่อรันด้วย native filesystem permissions ค่ะ จึงจัดเป็น environment restriction ไม่ใช่ product regression ในรอบนี้ค่ะ

Ignored-by-default tests คือ live Drive fixture และ OS keyring mutation เพราะแตะ external account/credential store ค่ะ รอบ audit นี้ไม่ได้รันสองรายการดังกล่าวค่ะ

## 5. Completed in This Alignment Round

- แยกหน้าที่เอกสารให้ `PROJECT_PLAN.md` เป็น direction/roadmap และไฟล์นี้เป็น operational handoff ค่ะ
- เปลี่ยน MVP checklist ที่กำกวมเป็น capability matrix ซึ่งแยก Usable, Prototype, Foundation only และ Missing ค่ะ
- แสดง execution order จริงว่า Phase 3 มาก่อนงาน Phase 2/4 ที่เหลือค่ะ
- ติดป้าย Phase 1 และ Phase 3A ว่า complete เฉพาะขอบเขต foundation/slice ค่ะ
- เปลี่ยน milestone ถัดไปเป็น R1 — Native Auth + Read-only Existing Drive Binding และ freeze งานที่ไม่ช่วย Safe Sync Alpha ค่ะ
- อัปเดต Sync Results จาก waiting-for-merge เป็น merged และย้าย OAuth configuration ออกจาก Phase 0 blockers ค่ะ
- ติดป้าย Demo/Phase 0 evidence เก่าให้เป็น historical หรือ pre-commit ตามจริงค่ะ
- ล็อก Personal First Release scope, Post-release scope และ execution order R1–R8 ตาม approval `Approve lock roadmap` ค่ะ
- เพิ่ม milestone dependencies, exit gates, verification matrix และ change-control rules ใน `PROJECT_PLAN.md` ค่ะ

## 6. Locked Roadmap Checkpoint

- Locked sequence คือ `R1 → R2 → R3 → R4 → R5 → R6 → R7 → R8` ค่ะ
- R1–R4 ส่งมอบ Safe Sync Alpha ค่ะ
- R5 ปิด Local Product Completion ค่ะ
- R6 ปิด Persistent Knowledge Core ค่ะ
- R7 บังคับ native runtime acceptance บน macOS, Windows, Ubuntu และ physical Android ค่ะ
- R8 ทำ recovery drill, release verification และ Personal First Release ค่ะ
- Active implementation milestone คือ R1 — Native Auth + Read-only Existing Drive Binding ค่ะ
- เปิด implementation milestone ได้ครั้งละหนึ่ง milestone และต้องผ่าน exit gate พร้อม approval ก่อน transition ค่ะ
- Planning range ที่เหลือจากผลรวม milestone คือ 10–19 focused engineering weeks โดยไม่รวมเวลารอ environment, device, external review หรือ account approval ค่ะ
- Scope, order และ exit gates ถูกล็อกค่ะ Planning range ไม่ใช่ deadline lock ค่ะ

## 7. Known Gaps and Direction Risks

### Product blockers

- ไม่มี production native auth integration และ exact Existing Drive binding ค่ะ
- ไม่มี production Drive read/write path หรือ cross-device end-to-end journey ค่ะ
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

1. รักษา active R1 branch และ uncommitted work ทั้งหมดก่อนแก้ไฟล์ค่ะ
2. ตรวจว่า R1 implementation plan และ approval state ใน active session ครอบคลุม read-only fixture/root, native credential boundary, production adapter boundary, rollback/cleanup และ acceptance ค่ะ
3. เดิน R1 ตาม locked scope โดยแยก approval ก่อนเปิด OAuth browser หรือใช้ live Google Drive fixture ค่ะ
4. ปิด R1 ด้วย exit-gate evidence บน source head เดียวกันและขอ approval ก่อน transition ค่ะ
5. ห้ามเริ่ม R2 upload/download, remote mutation หรือแตะ personal Existing Vault ก่อน R1 exit gate ผ่านค่ะ

## 9. Approval State

- Documentation audit, alignment, roadmap lock, PR review และ merge เข้า `main` ได้รับอนุมัติและดำเนินการแล้วค่ะ
- Locked scope/order/gates ได้รับ approval ด้วยข้อความ `Approve lock roadmap` เมื่อ 2026-07-14 และอยู่บน `main` ที่ `5160882` ค่ะ
- Active R1 branch มี implementation work แล้วค่ะ Session ที่ทำ R1 ต้องถือ approval state ของตัวเองเป็น source of truth และห้ามตีความ roadmap merge ว่าอนุมัติ live external access ค่ะ
- OAuth browser และ live Google Drive access ยังต้องมี explicit approval ตาม R1 plan ค่ะ
- ไม่มี approval ด้าน User Data Policy ค้างอยู่ค่ะ OAuth credential และ token ต้องอยู่ภายนอก repository และห้ามแสดงใน log ค่ะ
- งาน implementation ใหม่ต้องเสนอแผนและขออนุมัติคุณโอก่อนลงมือค่ะ

## 10. Evidence Index

- Phase 0 feasibility และ external gates อยู่ที่ [docs/phase-0/RESULTS.md](docs/phase-0/RESULTS.md) ค่ะ
- Local Demo และ macOS UAT อยู่ที่ [docs/demo/RESULTS.md](docs/demo/RESULTS.md) ค่ะ
- Sync Foundation architecture, acceptance และผลอยู่ใน [docs/sync](docs/sync) ค่ะ
- Engineering/release history อยู่ที่ [CHANGELOG.md](CHANGELOG.md) ค่ะ
