# Changelog

## Unreleased — R3.3 closure and R3.4 controlled handoff — 2026-07-16

- ปิด R3.3 Remote Mutation Block Enforcement ที่ source boundary
  `main@94cdffa` และ CI-boundary remediation `main@538fb72` ค่ะ Exact-head CI run
  `29494622309` ผ่านทั้ง `quality` และ `android-compile` ค่ะ
- ยืนยัน static/runtime block ว่าไม่มี executable production path สำหรับ existing-item
  remote content update, rename, move, Trash, HTTP `DELETE`, permission mutation หรือ
  generic provider request ค่ะ Blocked intent เก็บ exact evidence, จบที่
  `NeedsReconcile` และไม่ advance cursor ค่ะ
- R3.4 ทำ bounded read-only Desktop/Android SAF capability proof และ Sol High
  change-control แล้วค่ะ ไม่เกิด source/test edit หรือ local mutation capability ใหม่ค่ะ
- Desktop ยังพิสูจน์ durable exact source identity, atomic/no-replace replacement,
  final outcome และ durable watcher/replay echo suppression ไม่ได้ค่ะ Android SAF ยัง
  พิสูจน์ held destination-parent identity, complete collision set, atomic no-replace
  publication และ final outcome ไม่ได้ค่ะ
- R3.4 จึงมีสถานะ `open / blocked by prerequisites` ไม่ใช่ completed ค่ะ R3.5 ต้อง
  รับ controlled durable identity/journal/recovery/echo prerequisites ก่อน แล้วจึง
  กลับมารัน R3.4 completion gate ได้ค่ะ

## Unreleased — R3 planning prepared — 2026-07-15

- ยืนยัน R2 implementation, PR #27 merge `94db388` และ post-merge Quality run
  `29429364407` ว่า complete ตาม locked milestone scope ค่ะ
- เพิ่ม planning pack `R3.0–R3.7` พร้อม safety boundary, dependencies, component
  gates, conflict matrix, parallel file ownership และ exact-head closure ค่ะ
- เพิ่ม GPT/Antigravity model routing, usage vocabulary, per-run ledger และ
  efficiency review โดยไม่อ้าง token/model pin ที่ execution surface ไม่รายงานค่ะ
- R3 source implementation ยังไม่ active ค่ะ ต้อง merge planning/closure checkpoint
  และได้รับ transition approval แยกต่างหากก่อนค่ะ

## Unreleased — R2 guarded transfer completed — 2026-07-15

Built from the R1 merge checkpoint `681271a` and merged into `main` via PR #27
at `94db388`ค่ะ Disposable macOS byte-exact round trip และ Android API 36 live
acceptance ผ่านแล้วค่ะ Final documentation head `b08bb20` passed Quality run
`29427668835` and platform run `29427668933`ค่ะ Post-merge Quality run
`29429364407` passed on `main`ค่ะ R2 is complete for its locked milestone scope
onlyค่ะ

- เพิ่ม schema v3 durable upload/download evidence, create-only operation
  markers, retry scheduling, `AuthRequired`, `NeedsReconcile`, restart recovery,
  cursor-gated mutation batches และ redacted transfer historyค่ะ
- เพิ่ม narrow Drive transfer capability สำหรับ exact-root create/resumable
  upload, status/reconciliation และ bounded exact-ID blob download โดยไม่มี
  DELETE, Trash, rename, move, permission mutation หรือ blind overwrite pathค่ะ
- เพิ่ม private staged payload/content-addressed base publication, byte/hash
  verification และ guarded create-no-replace local publication สำหรับ desktop
  กับ Android SAFค่ะ
- เพิ่ม desktop local observation และ bounded Android SAF inventory/hints พร้อม
  protected-path, duplicate-path, size-bound และ stale-capability fail-closed
  policyค่ะ
- เปลี่ยน Android binary bridge เป็น transcript-checked 192 KiB chunks เพื่อ
  ไม่ส่ง payload 16 MiB เป็น Base64 message เดียว และตรวจ offset, length,
  SHA-256, EOF กับ native readback ทุกครั้งค่ะ
- เปลี่ยน Android private base publication จาก hard link ที่ SELinux ปฏิเสธ
  เป็น durable private copy + exact verification + create-no-replace rename
  พร้อม crash recovery ที่รักษา ambiguous evidenceค่ะ
- แก้ Android no-backup `O_PATH` directory fsync ด้วย capability-relative
  syncable reopen และคง private state/stage durability แบบ fail-closedค่ะ
- เพิ่ม single-owned guarded worker, bounded 100-page/1,000-operation runs,
  resumable 8 MiB chunks, desktop 512 MiB payload cap และ Android 16 MiB capค่ะ
- แก้ restart upload ให้ restage ได้เฉพาะ partial private stage ที่พิสูจน์ว่า
  operation/source ตรงครบ และเก็บ wrong/full/hardlinked/replaced evidence แบบ
  fail-closedค่ะ
- แก้ offline retry storm ด้วย immutable eligibility snapshot ต่อ guarded run
  และ post-execution retry clock ที่ใช้เหมือนกันบน native กับ Androidค่ะ
- ขยาย R2 offline aggregate และ CI ให้ครอบคลุม regression crates,
  Android-target strict Clippy, Kotlin policy tests, APK build/alignment และ
  platform packagesค่ะ
- macOS A → Drive → B และ Android API 36 A/B/C ผ่าน byte-exact disposable
  journeys รวมไฟล์ Unicode, zero-byte, 6 MiB + 1, 12 MiB และ 15 MiBค่ะ Android
  offline injection ไม่สร้าง remote duplicate และ cold restart ฟื้น durable
  pending/reconcile state จนกลับ readyค่ะ Physical Android ยัง deferred ไป R7ค่ะ
- macOS restart upload/download, offline pause/resume, Keychain restoration และ
  confirmed disconnect/reconnect ผ่านค่ะ Disconnect เก็บ exact binding กับ
  durable transfer history ไว้ครบ และ reconnect บัญชี/รากเดิมกลับ readyค่ะ
- Local final aggregate, Android cross-target Clippy, Kotlin tests, APK build และ
  16 KiB alignment ผ่านค่ะ Final APK คือ 304,163,423 bytes, SHA-256
  `cfb77292713957e245889c564ba6d1717303c0eca26f014b58696506bea02f1c`
  และผ่าน v2 signatureค่ะ Evidence head `cba94d1` ผ่าน Quality run
  `29424661478` และ platform run `29424659698` ครบทั้ง Quality, Android,
  Ubuntu AppImage และ Windows NSIS ค่ะ Quality attempt แรกล้มจาก GitHub runner
  disk-full และ clean rerun ผ่านโดยไม่แก้ sourceค่ะ

## Unreleased — R1 native auth and read-only binding — 2026-07-14

Merged to `main` via PR #26 at `681271a`ค่ะ

- เชื่อม desktop native OAuth/OS credential store และ Android Google
  authorization bridge โดยไม่ส่ง token เข้า WebView, SQLite หรือ logค่ะ
- เพิ่ม production GET-only Drive adapter, exact account/root binding,
  start-token-before-scan, bounded recursive inventory, Changes drain และ
  restart restorationค่ะ
- เพิ่ม redacted Tauri Sync connect/bind/scan/preview/status/disconnect commands
  โดย R1 ไม่มี Drive mutation pathค่ะ
- R1 live disposable acceptance, Quality, Android compile/emulator, Ubuntu
  AppImage และ Windows NSIS ผ่านบน candidate เดียวก่อน mergeค่ะ

## Unreleased — Phase 3A sync foundation — 2026-07-13

Merged to `main` via PR #23 at `db85177` on 2026-07-14ค่ะ This engineering milestone has not yet shipped in a user-facing releaseค่ะ

- สร้าง production `myvault-sync-engine` แยกจาก Phase 0 Drive fixture harness ค่ะ Phase 3A ไม่มี OAuth, network request หรือ live Drive mutation ค่ะ
- เพิ่ม private per-Vault SQLite schema v1 สำหรับ exact remote-root binding, initial scan/cursors, duplicate-preserving remote entries, nullable base state, durable queue, redacted history และ incremental cursor batches ค่ะ
- เพิ่ม typed MD5/SHA-1/SHA-256 remote checksums, canonical portable paths และ protected `.obsidian`/`.trash` rejection ค่ะ
- เพิ่ม start-token-before-scan orchestration, exact scan-page cursor, transactional Changes drain และ durable cursor publication หลัง local commit เท่านั้นค่ะ
- เพิ่ม durable completed-operation tombstones เพื่อคง exact idempotency หลัง completion, exact remote-ID requirements, move destination metadata และ restart recovery ที่เปลี่ยน interrupted `Running` jobs เป็น `NeedsReconcile` เพื่อห้าม blind duplicate upload ค่ะ
- เพิ่ม exclusive per-Vault Sync lease เพื่อห้าม live worker ตัวที่สองทำ false restart recovery ค่ะ
- เพิ่ม crash-aware local mutation state `Pending` → `Applying` → `Committed`, partial-abort protection และ exact schema-definition validation สำหรับ CHECK, UNIQUE และ foreign-key contracts ค่ะ Version-zero migration ตรวจ user schema objects ทุกชนิดและ validate ใน transaction ก่อน commit ส่วน schema version ติดลบถูกเก็บเป็น evidence และ reject แบบ fail closed ค่ะ
- เพิ่ม Phase 3A architecture/acceptance/results และ CI gates สำหรับ fmt, strict Clippy, native Linux tests และ native Windows compile-only โดยไม่อ้าง Windows runtime acceptance ค่ะ
- Phase 3A isolated suite ผ่าน 17 tests และ `pnpm test:rust` ผ่าน Tauri, Core, Desktop Auth, Drive spike และ Sync Foundation matrices ค่ะ

## Unreleased — Phase 1 local implementation closure — 2026-07-13

Captured by commits `66c299f` and `cbde0c1` on `main`ค่ะ This engineering milestone has not yet shipped in a user-facing releaseค่ะ

- เชื่อม immutable pre-save recovery snapshots เข้ากับ desktop guarded-save runtime ค่ะ Configured snapshot failure คืน stable `recoveryUnavailable` และหยุด save ก่อน Vault mutation ส่วน stale save ไม่ publish recovery snapshot ค่ะ
- เพิ่ม native recursive Vault watcher และ debounced explorer refresh ที่ผูกกับ opaque session ค่ะ
- เพิ่ม Android SAF document-tree Vault activation พร้อม persisted native-only capability, revision-checked guarded save และ explicit `directorySyncUnsupported`/`writeOutcomeUnknown` สำหรับ contract ที่ไม่เทียบเท่า desktop atomic publication ค่ะ
- ยืนยัน Phase 1 SQLite derived-index schema/migration/rebuild contract และแยก persistent content index เป็น milestone ถัดไปค่ะ
- Google OAuth configuration และ live Drive fixture round trip ผ่านแล้วค่ะ Production Drive Sync ยังเป็น Phase 3 ค่ะ
- Pre-commit Phase 1 local implementation closure checkpoint เมื่อ 2026-07-13 ผ่าน frontend 24 tests, Tauri 8 tests, app-service 14 tests (2 unit + 12 integration), snapshot 62 tests และ SAF policy 3 tests พร้อม strict host/Android aarch64 Clippy, frontend production build, full Android debug APK build และ macOS debug application bundle ค่ะ Frontend build ยังมี non-blocking large-chunk warning ค่ะ
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
