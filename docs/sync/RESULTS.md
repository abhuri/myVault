# Phase 3 Sync Evidence

Updated 2026-07-15 Asia/Bangkok ค่ะ

## R2 — Guarded Transfer

Status: `FINAL CANDIDATE — LIVE ACCEPTANCE PASSED; FINAL HEAD/CI PENDING` ค่ะ

R2 started from the merged R1 checkpoint `681271a` on branch
`codex/r2-guarded-transfer` after one-time execution approval from คุณโอค่ะ The
candidate implements durable byte-verified upload/download and Android SAF
runtime integrationค่ะ Live macOS byte-exact round trip and API 36 emulator
acceptance now pass, but R2 is not complete until the remaining macOS lifecycle,
current dirty working tree, fresh exact-HEAD CI, final review, Draft PR #27
readiness and merge close ตาม [R2_ACCEPTANCE.md](R2_ACCEPTANCE.md)ค่ะ

### Implemented candidate scope

- Schema v3 records exact upload/download intent, immutable operation marker,
  expected revisions, SHA-256/length/MIME, private stage/base reference, durable
  retry state and redacted outcomes before side effectsค่ะ Interrupted `Running`
  work reopens as `NeedsReconcile` rather than replaying blindlyค่ะ
- Production Drive transfer code is limited to exact-root metadata/media reads,
  create/resumable-init POST and resumable-session PUTค่ะ It re-verifies account,
  root ancestry, exact parent/file identities, hash/length/revision and lost
  final responses before deciding whether another create is safeค่ะ
- Guarded local publication is create-no-replaceค่ะ Same-byte targets are
  verified no-ops; differing or unknown outcomes preserve evidence and stop at
  `NeedsReconcile` without replacementค่ะ
- Desktop payloads are streamed and capped at 512 MiBค่ะ Android SAF payloads
  are capped at 16 MiB and cross the Rust/Kotlin bridge in transcript-checked
  192 KiB chunks through one stateful read session, never as one whole Base64
  messageค่ะ Holding one provider stream makes the read path O(n) in payload
  size rather than reopening and rereading every prefixค่ะ The session binds the
  exact root, portable path and document ID, while owner/foreign transcript
  isolation prevents an unrelated or malformed contender from aborting the
  active ownerค่ะ Upload chunks are 8 MiB and each run is capped at 1,000
  operations and 100 Changes pagesค่ะ
- Android private base publication uses a private copy, file and directory
  durability checks, exact identity/hash verification, and `renameat2`
  create-no-replaceค่ะ Pending source identity, link count, metadata, bytes and
  hash are revalidated after the final pre-rename fault boundary, and the named
  final file is reopened, verified and fsynced before stage cleanupค่ะ This
  closes the pending source-swap window while retaining crash evidence and
  immutable final objectsค่ะ
- Android no-backup roots opened as `O_PATH` capabilities are reopened relative
  to the held descriptor as syncable read-only directories before `fsync`ค่ะ
  The private Sync/stage store therefore fails closed instead of treating
  `EBADF` as weaker SAF durabilityค่ะ
- Changes cursor advancement is transactional with declared local mutation
  completion and durable remote transfer completionค่ะ Expired/ambiguous cursors,
  remote moves/removals/renames, protected paths and duplicate exact paths force
  a durable full rescanค่ะ
- Frontend receives opaque sessions/operation IDs and redacted status onlyค่ะ
  Tokens, resumable session URIs, provider bodies, content bodies and ambient
  Vault paths remain outside frontend DTOs, logs and SQLiteค่ะ

### Integrated deterministic and native evidence

- The post-live integrated working tree passed `pnpm quality:r2:offline` on
  2026-07-15 Asia/Bangkokค่ะ This includes frontend typecheck, 5 files/40 tests,
  production build, Rustfmt, strict Clippy, and all expanded R2/regression testsค่ะ
- A final macOS debug application and DMG bundle built successfully from the
  same working tree after the live fixesค่ะ
- Key final Rust counts include Drive 53, private-root 18, Vault SAF 10,
  transfer 14, and Tauri 56 testsค่ะ The matrix includes real SQLite transaction
  aborts, exact staged/base durability failures, restart recovery, and every
  emitted 8 MiB resumable upload/status boundary for 0, 1, 8 MiB, 8 MiB + 1,
  and 16 MiB payloadsค่ะ
- Android aarch64 strict Clippy and Gradle Vault SAF unit tests passedค่ะ The
  final API 36 debug APK is 304,052,519 bytes, passed `zipalign` verification
  for 16 KiB page alignment, and built successfully after the final source
  fixesค่ะ
- Final local APK SHA-256 is
  `a3a1cef9b2a4e3e08118cef2ce4209d76578e6fb366edbc395e566369070bd4a`ค่ะ
- macOS live acceptance used only disposable Local Vaults and one exact
  disposable Drive rootค่ะ Markdown, Thai Unicode paths, zero-byte files,
  6 MiB + 1 byte and two 15 MiB restart fixtures completed a byte-exact
  Local A → Drive → Local B journeyค่ะ
- Android API 36 Vault A converged to `ready` with 9 files and no active,
  pending, retry, auth-required, or reconcile workค่ะ Vault B downloaded the
  same 9-file manifest byte-exactly; its recursive manifest SHA-256 was
  `afda517b358b185071d90cd6a91c457f042202f5ee6af03068a59786227fec01`ค่ะ
- A 12 MiB upload was interrupted after its private durable stage by disabling
  emulator networking, then completed after connectivity restorationค่ะ The
  remote metadata contained exactly one matching fixture, proving no duplicate
  create across the offline boundaryค่ะ
- Android Vault C was force-stopped after the first recovered fileค่ะ Relaunch
  restored the exact SAF root and Drive binding as 1 completed, 8 pending and
  1 reconcile operation; same-account authorization reacquisition resumed the
  work to `ready` with all counters zeroค่ะ Vault B and C then matched byte for
  byte across 10 files with manifest SHA-256
  `e3782952e5eebc7ce2919a858fe59c1b1c2cd4db82a9ba22bb7958d6c446542c`ค่ะ
- The final rebuilt APK then downloaded the same exact-root fixture into empty
  Android Vault D through the stateful SAF write transcriptค่ะ All 10 files
  completed with zero active, pending, retry, authorization or reconcile work,
  and Vault D's per-path SHA-256 manifest exactly equaled Vault Cค่ะ A cold
  force-stop/relaunch reconnected the same binding and returned to `ready` with
  every queue counter still zeroค่ะ
- Static Drive mutation/token audits found no reachable DELETE, Trash, rename,
  move, permission mutation, generic request API, durable bearer capability or
  production dependency on `drive-sync-spike`ค่ะ
- `pnpm audit --prod` reported no known vulnerabilitiesค่ะ
- Draft PR #27 candidate `ed90bfb` passed quality run `29357617209`ค่ะ The
  `quality` job completed in 14m26s and `android-compile` completed in 7m26s,
  including the Linux Rust fault matrix, static trust-boundary audit, Android
  APK build, Kotlin native-root tests and 16 KiB alignmentค่ะ
- Platform run `29357617372` passed Ubuntu 22.04 AppImage in 11m27s and Windows
  2022 NSIS in 12m19s on the same candidateค่ะ These are package/build claims,
  not native UI acceptance claimsค่ะ

### Deliberately pending / finalization work

- The current evidence is from a dirty working tree based on branch HEAD
  `e39d131`; the final candidate commit does not exist yetค่ะ Earlier green CI
  on `ed90bfb` cannot satisfy the final-head gateค่ะ
- Live desktop auth-expiry/disconnect was not forced because disconnecting the
  native credential requires a fresh action-time confirmationค่ะ Deterministic
  auth-expiry, refresh, retry, and idempotent disconnect suites pass, while the
  broader macOS scenario checkbox remains open and must be described honestlyค่ะ
- No personal Drive item, personal Vault, raw credential, 2FA flow, or physical
  Android device was accessedค่ะ Physical-device acceptance remains R7ค่ะ
- Draft PR #27 must remain Draft until the final commit is pushed, fresh Quality,
  Android, Ubuntu, and Windows checks pass, and final security/diff review finds
  no blockerค่ะ

## R1 — Native Auth + Read-only Existing Drive Binding

Status: `COMPLETE — MERGED VIA PR #26` ค่ะ

R1 source was merged into `main` at `681271a` on 2026-07-14 after live
disposable read-only acceptance and Quality, Android compile/emulator, Ubuntu
AppImage and Windows NSIS checks passed on the same candidateค่ะ R1 connected
native desktop/Android authorization, production GET-only Drive access, exact
account/root binding, recursive scan, Changes drain, restart restoration and
redacted Tauri status without exposing Drive mutation operationsค่ะ

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
