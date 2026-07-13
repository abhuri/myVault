# Changelog

## Unreleased — Phase 3A sync foundation — 2026-07-13

- สร้าง production `myvault-sync-engine` แยกจาก Phase 0 Drive fixture harness ค่ะ Phase 3A ไม่มี OAuth, network request หรือ live Drive mutation ค่ะ
- เพิ่ม private per-Vault SQLite schema v1 สำหรับ exact remote-root binding, initial scan/cursors, duplicate-preserving remote entries, nullable base state, durable queue, redacted history และ incremental cursor batches ค่ะ
- เพิ่ม typed MD5/SHA-1/SHA-256 remote checksums, canonical portable paths และ protected `.obsidian`/`.trash` rejection ค่ะ
- เพิ่ม start-token-before-scan orchestration, exact scan-page cursor, transactional Changes drain และ durable cursor publication หลัง local commit เท่านั้นค่ะ
- เพิ่ม durable completed-operation tombstones เพื่อคง exact idempotency หลัง completion, exact remote-ID requirements, move destination metadata และ restart recovery ที่เปลี่ยน interrupted `Running` jobs เป็น `NeedsReconcile` เพื่อห้าม blind duplicate upload ค่ะ
- เพิ่ม exclusive per-Vault Sync lease เพื่อห้าม live worker ตัวที่สองทำ false restart recovery ค่ะ
- เพิ่ม crash-aware local mutation state `Pending` → `Applying` → `Committed`, partial-abort protection และ exact schema-definition validation สำหรับ CHECK, UNIQUE และ foreign-key contracts ค่ะ Version-zero migration ตรวจ user schema objects ทุกชนิดและ validate ใน transaction ก่อน commit เพื่อรักษา malformed evidence ค่ะ
- เพิ่ม Phase 3A architecture/acceptance/results และ CI gates สำหรับ fmt, strict Clippy, native Linux tests และ native Windows compile-only โดยไม่อ้าง Windows runtime acceptance ค่ะ
- Phase 3A isolated suite ผ่าน 16 tests และ `pnpm test:rust` ผ่าน Tauri, Core, Desktop Auth, Drive spike และ Sync Foundation matrices ค่ะ

## Unreleased — Phase 1 local implementation closure — 2026-07-13

- เชื่อม immutable pre-save recovery snapshots เข้ากับ desktop guarded-save runtime ค่ะ Configured snapshot failure คืน stable `recoveryUnavailable` และหยุด save ก่อน Vault mutation ส่วน stale save ไม่ publish recovery snapshot ค่ะ
- เพิ่ม native recursive Vault watcher และ debounced explorer refresh ที่ผูกกับ opaque session ค่ะ
- เพิ่ม Android SAF document-tree Vault activation พร้อม persisted native-only capability, revision-checked guarded save และ explicit `directorySyncUnsupported`/`writeOutcomeUnknown` สำหรับ contract ที่ไม่เทียบเท่า desktop atomic publication ค่ะ
- ยืนยัน Phase 1 SQLite derived-index schema/migration/rebuild contract และแยก persistent content index เป็น milestone ถัดไปค่ะ
- Google OAuth configuration และ live Drive fixture round trip ผ่านแล้วค่ะ Production Drive Sync ยังเป็น Phase 3 ค่ะ
- Final uncommitted Phase 1 local implementation closure checkpoint เมื่อ 2026-07-13 ผ่าน frontend 24 tests, Tauri 8 tests, app-service 14 tests (2 unit + 12 integration), snapshot 62 tests และ SAF policy 3 tests พร้อม strict host/Android aarch64 Clippy, frontend production build, full Android debug APK build และ macOS debug application bundle ค่ะ Frontend build ยังมี non-blocking large-chunk warning ค่ะ
- Android debug APK SHA-256 คือ `ace5ca1504ea06a0964a67904172b21d1babc2630b999e3ea18b9a803fd20a5f` ค่ะ 16 KB zip alignment และ APK Signature Scheme v2 ผ่านค่ะ Windows/Ubuntu native runtime และ physical Android evidence ยัง deferred ค่ะ
- Live Copy-of-Vault UAT บน macOS ผ่านตาม runbook แล้วค่ะ Native picker, watcher clean reload, dirty-buffer conflict stop, explicit reload, guarded saves สาม revision, recovery snapshots แบบ byte-exact, Reader keyboard navigation, Mermaid failure isolation และ close/reopen continuity ผ่านโดยใช้เฉพาะ disposable Vault ใต้ `/tmp` ค่ะ Windows/Ubuntu native runtime และ physical Android evidence ยังคง deferred ค่ะ
- ปรับ evidence chronology ให้ pre-SAF baseline แยกจาก current SAF acceptance และยืนยันว่า Production Drive Sync อยู่ Phase 3 ค่ะ

## v0.1.0-demo — 2026-07-12

- เพิ่ม macOS-first Local Desktop Demo และ Synthetic Demo Vault ค่ะ
- เพิ่ม native folder picker, opaque Vault session และ bounded explorer ค่ะ
- เพิ่ม coherent Markdown read และ revision-checked atomic save พร้อม autosave 750 ms ค่ะ
- เพิ่ม Edit/Read mode, GFM tables/tasks, code highlighting, strict Mermaid และ sanitized reader ค่ะ
- เพิ่ม filter, quick switcher, outline, opened-note backlinks และ graph prototype ค่ะ
- เพิ่ม responsive compact drawers พร้อม keyboard focus containment ค่ะ
- เพิ่ม stale-revision conflict stop และ explicit reload recovery ค่ะ
- Google Drive Sync, physical Android acceptance และ store distribution ยัง deferred ค่ะ
