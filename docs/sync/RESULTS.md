# Phase 3 Sync Evidence

Updated 2026-07-14 Asia/Bangkok ค่ะ

## Phase 3A — Sync Foundation

Status: `COMPLETE — MERGED VIA PR #23` ค่ะ

Source head `7f5b8d6` ถูก merge เข้า `main` ที่ `db85177` เมื่อ 2026-07-14 ค่ะ Post-merge Quality run `29270038450` ผ่านทั้ง `quality` และ `android-compile` ค่ะ สถานะ Complete นี้หมายถึงขอบเขต Sync Foundation เท่านั้น โดย production OAuth/Drive adapter, Tauri integration และ user-visible Sync ยังไม่รวมค่ะ

### Implemented

- สร้าง production foundation crate `myvault-sync-engine` แยกจาก config-gated `drive-sync-spike` acceptance harness ค่ะ
- เพิ่ม typed native `DriveClient` boundary ที่รับเฉพาะ remote metadata/pages และ redacted error code โดยไม่มี credential หรือ payload body ใน API ค่ะ
- เพิ่ม private per-Vault SQLite schema v1 สำหรับ exact remote-root binding, initial-sync phases/cursors, duplicate-preserving remote metadata, nullable base revisions/hashes, durable queue, redacted history และ incremental change batches ค่ะ
- Remote checksums แยก algorithm เป็น MD5, SHA-1 และ SHA-256 พร้อม canonical lowercase/length validation ค่ะ
- Initial sync บังคับ start-token-before-scan, exact scan-page cursor, transactional page application, Changes drain และ final durable cursor publication ค่ะ
- Durable queue รองรับ upload, download, move และ Trash metadata โดยไม่เก็บ note/attachment body หรือ OAuth token ค่ะ Completed operation คงเป็น non-runnable tombstone ทำให้ exact retry หลัง completion ไม่ทำงานซ้ำ และ mismatched ID reuse fail closed ค่ะ Download, Move และ Trash บังคับ exact remote file ID ค่ะ
- Exclusive per-Vault OS-level lease ถูก acquire ก่อนเปิด SQLite และถือไว้ตลอดอายุ store ค่ะ Live worker ตัวที่สองถูก reject โดยไม่เปลี่ยน queue ส่วน retained `Running` jobs จะเปลี่ยนเป็น `NeedsReconcile` หลัง process เดิมปล่อย lease แล้วเท่านั้นค่ะ
- Incremental cursor batch ใช้ durable `Pending` → `Applying` → `Committed` state ค่ะ Crash ระหว่าง local operation จะค้างที่ `Applying` และบังคับ reconciliation ก่อน retry, abort หรือ cursor commit ค่ะ
- Newer, negative-version, partial, constraint-weakened และ corrupt database evidence ถูกเก็บไว้และ fail closed โดยไม่มี automatic delete/rebuild ค่ะ Version-zero migration ตรวจ user table/index/view/trigger ทั้งหมดและ validate exact schema ใน transaction ก่อน commit ค่ะ
- เพิ่ม new-crate fmt, strict Clippy และ test commands ใน local `test:rust` กับ quality CI ค่ะ Platform CI รัน suite บน native Linux และ compile tests บน native Windows โดยไม่อ้าง Windows runtime acceptance ค่ะ

### Verification

- `cargo test --manifest-path crates/myvault-sync-engine/Cargo.toml` ผ่าน 17 integration tests ค่ะ
- `cargo clippy --manifest-path crates/myvault-sync-engine/Cargo.toml --all-targets -- -D warnings` ผ่านค่ะ
- `pnpm test:rust` ผ่าน Tauri 8 tests, myvault-core suites, Desktop Auth 9 tests, Drive spike 25 tests และ Sync Foundation tests ค่ะ Live Drive test และ OS keyring mutation test ยังคง ignored by default ตาม contract ค่ะ
- Source head `7f5b8d6` ผ่าน Quality, Android Compile, Ubuntu AppImage และ Windows NSIS ก่อน merge ค่ะ Merge commit `db85177` ผ่าน post-merge Quality run `29270038450` ค่ะ
- Phase 3A ไม่ได้เรียก OAuth, Google Drive network, personal Vault หรือ credential ใดค่ะ

### Deferred to Phase 3B+

- Desktop OAuth runtime/token exchange และ Android authorization-provider unification ค่ะ
- Production Google Drive REST adapter และ read-only Existing Drive folder binding ค่ะ
- Guarded upload/download, retry/backoff และ unknown-outcome reconciliation ค่ะ
- Rename/move/Trash, attachments, three-way merge และ conflict copies ค่ะ
- Tauri Sync commands, UI, history presentation และ diagnostics export ค่ะ
- Windows private-root provisioning และ Sync runtime acceptance ค่ะ Phase 3A มี native compile-only gate ค่ะ
