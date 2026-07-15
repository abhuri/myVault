# R3 — Remote Mutations and Conflict Safety Acceptance

Owner: Sunday ค่ะ

Current status: `PLANNED — NOT ACTIVATED` ค่ะ

R3 ถือว่า complete เมื่อทุก applicable checkbox ด้านล่างมี evidence จาก exact
candidate HEAD เดียวค่ะ Mock, compile, emulator, native runtime และ live evidence
ต้องถูกแยกชื่ออย่างตรงไปตรงมาค่ะ Source implementation ห้ามเริ่มจนกว่า Gate 0
ผ่านและคุณโออนุมัติ transition เข้า R3 ค่ะ

## Gate 0 — R3.0 activation and contract freeze

- [x] R2 source candidate ถูก merge ผ่าน PR #27 ที่ `94db388` ค่ะ
- [x] Post-merge Quality run `29429364407` ผ่านบน `main@94db388` ค่ะ
- [ ] R2 documentation closure `f7a0d7c` และ R3 planning pack อยู่บน canonical
  `main` หรือ equivalent approved checkpoint ค่ะ
- [ ] `R3_PLAN.md`, acceptance, usage contract, Project Plan, README และ Session
  Handoff ผ่าน cross-document review ค่ะ
- [ ] Conflict matrix, mutation allowlist, folder topology, merge policy,
  conflict-copy identity/naming และ unknown-outcome taxonomy ถูก freeze ค่ะ
- [ ] Official Drive mutation semantics และ precondition limits ถูกบันทึกค่ะ
- [ ] Disposable account/root และ two-device local fixtures ถูก allowlist โดยไม่
  เปิดเผย credential หรือ personal path ค่ะ
- [ ] คุณโออนุมัติ transition เข้า R3 อย่างชัดเจนค่ะ

## Gate 1 — R3.1 durable mutation and conflict evidence

- [ ] Schema v3 migrate ไป schema รุ่น R3 แบบ transactional โดยรักษา binding,
  cursor, queue, history, transfer และ base evidence ค่ะ
- [ ] Newer, negative, malformed, partial หรือ constraint-weakened schema ถูก
  preserve และ reject โดยไม่ repair อัตโนมัติค่ะ
- [ ] Mutation evidence มี exact IDs/parents/paths, expected revisions, base ref,
  operation marker, durable phase, retry state และ redacted outcome ค่ะ
- [ ] Conflict evidence มี classification, conflict-copy operation identity และ
  bounded explanation metadata โดยไม่ใช้ timestamp เป็น correctness input ค่ะ
- [ ] SQLite ไม่มี credential, provider body, content body, ambient path หรือ
  bearer-like capability ค่ะ
- [ ] Restart หลัง durable-intent boundary จบเป็น verified completion,
  retry-safe state หรือ `NeedsReconcile` ค่ะ
- [ ] Cursor ถูกกั้นจน mutation, merge/conflict publication และ base publication
  commit ครบค่ะ
- [ ] Exact operation retry idempotent และ mismatched ID reuse fail closed ค่ะ

## Gate 2 — R3.2 conflict classification and materialization

- [ ] Pure classifier ไม่ผูกกับ Tauri, network หรือ platform provider ค่ะ
- [ ] Local-only, remote-only, both-changed, delete/edit, rename/edit,
  move collision, duplicate path และ offline replay มี typed result ครบค่ะ
- [ ] Markdown non-overlap merge deterministic และรักษา bytes/newline/frontmatter
  ตาม frozen contract ค่ะ
- [ ] Overlap, missing base, invalid encoding, oversized input และ ambiguous
  lineage สร้าง conflict-copy plan หรือ `NeedsReconcile` ค่ะ
- [ ] Binary both-changed รักษาทั้งสองเวอร์ชันเสมอค่ะ
- [ ] Rename/move cycle, case-only rename, destination collision, normalization,
  case folding และ parent-folder race มี explicit outcome ค่ะ
- [ ] Conflict-copy naming portable, deterministic, collision-safe และ exact
  rerun ไม่สร้างสำเนาซ้ำค่ะ
- [ ] Conflict copy ไม่ถูกลบอัตโนมัติค่ะ
- [ ] Merge/materialization วางแผน base publication ใหม่อย่าง explicit ค่ะ

## Gate 3 — R3.3 exact-ID Drive mutation

- [ ] Production mutation capability แยกจาก read-only และ create-only transfer
  capability ค่ะ
- [ ] Existing-content update ใช้ exact file ID, expected remote revision,
  immutable base hash และ intended hash/size ค่ะ
- [ ] Existing-content update ทำได้เฉพาะเมื่อ remote ยังตรง base ค่ะ Otherwise
  ต้องกลับเข้า classifier โดยไม่ overwrite ค่ะ
- [ ] Rename ใช้ exact file ID ค่ะ Move ใช้ exact file/old-parent/new-parent IDs ค่ะ
- [ ] Trash ใช้ exact file ID และตั้ง Trash เท่านั้นค่ะ
- [ ] ทุก request re-verifies account, root, ancestry, parents และ expected remote
  revision ก่อน side effect ค่ะ
- [ ] ทุก successful response ถูก post-verify ด้วย exact metadata ค่ะ
- [ ] Lost response ระหว่าง content/metadata mutation ถูก metadata/hash-reconcile
  ก่อน retry ค่ะ
- [ ] Shortcut, Google-native ambiguity, multiple parent, outside-root, malformed
  metadata, redirect และ origin change fail closed ค่ะ
- [ ] Supporting folder topology ถูกจำกัดตาม R3.0 และไม่กลายเป็น generic folder
  Sync capability ค่ะ
- [ ] Captured requests ตรง allowlist และ production surface ไม่มี HTTP `DELETE`,
  permission mutation หรือ generic request method ค่ะ
- [ ] 401/403/404/410/429/5xx, timeout และ unknown outcome ทำตาม frozen taxonomy ค่ะ

## Gate 4 — R3.4 guarded local mutation

- [ ] Desktop reuse revision checks, mutation service และ recovery journal ค่ะ
- [ ] Sync-owned rename, move, Vault-local Trash, guarded replacement และ
  conflict-copy publication recheck source/destination/revision ก่อน publish ค่ะ
- [ ] Destination collision หรือ changed identity รักษาทั้งสองฝั่งและ fail closed ค่ะ
- [ ] Case-only rename ผ่าน supported filesystems โดยไม่ overwrite ค่ะ
- [ ] Android SAF bind exact held root/document identity สำหรับ mutation ค่ะ
- [ ] Provider ที่พิสูจน์ atomicity/outcome ไม่ได้คืน `WriteOutcomeUnknown` หรือ
  `NeedsReconcile` ตามจริงค่ะ
- [ ] ไม่มี binary in-place overwrite shortcut ค่ะ
- [ ] Watcher/SAF echo ไม่ enqueue duplicate mutation ค่ะ
- [ ] Desktop fault matrix, Android fake-provider matrix และ emulator restart
  ผ่านตาม platform claim ค่ะ

## Gate 5 — R3.5 orchestration and reconciliation

- [ ] Local rename/move/Trash observation เข้า durable queue โดยไม่ใช้ watcher
  เป็น source of truth ค่ะ
- [ ] Remote rename/move/removal ผ่าน classifier และ guarded mutation ค่ะ
- [ ] Execution order เป็น stage/read → classify → durable intent → side effect
  → post-verify → base publish → completion → cursor commit ค่ะ
- [ ] Worker หนึ่งตัวต่อ Vault และไม่ถือ app/store lock ข้าม network/large I/O ค่ะ
- [ ] Offline, auth, retry, stale session และ redaction reuse R2 taxonomy ค่ะ
- [ ] Duplicate retry, watcher echo และ repeated Changes page ไม่สร้าง duplicate
  mutation หรือ conflict copy ค่ะ
- [ ] Safe merge/conflict result converge ด้วย guarded local replacement,
  guarded exact-ID remote update หรือ create-only conflict-copy upload ค่ะ
- [ ] Restart ทุก persistent boundary ไม่ทำให้ lost conflict copy หรือ cursor drift ค่ะ
- [ ] Frontend เห็นเฉพาะ redacted minimum status และไม่มี R4 control-plane scope ค่ะ

## Gate 6 — R3.6 deterministic and live acceptance

- [ ] `quality:r3:offline` หรือ equivalent frozen aggregate ผ่าน frontend,
  Rustfmt, strict Clippy, unit, integration, migration, property, fault และ doc tests ค่ะ
- [ ] R1–R2 regression matrix ยังคงผ่านค่ะ
- [ ] macOS disposable two-device journey ผ่าน rename, move, Trash, safe merge,
  conflict copy และ recursive byte/hash manifest ค่ะ
- [ ] Restart ระหว่าง remote/local mutation, merge, conflict copy, base publish
  และ pre-cursor commit ผ่านค่ะ
- [ ] Offline two-sided edits replay ตาม matrix โดยไม่สูญข้อมูลค่ะ
- [ ] Remote Trash exact identity, no permanent delete และ no auto-delete conflict
  copy ถูกตรวจทั้ง static และ live ค่ะ
- [ ] Android API 36 emulator ผ่านเฉพาะ supported mutation/conflict contract ค่ะ
- [ ] Ubuntu/Windows compile, test และ packaging ผ่านโดยไม่อ้าง native UAT ค่ะ
- [ ] Evidence ระบุ source HEAD, dirty state, environment, command, result และ
  deliberately untested behavior ค่ะ

## Gate 7 — R3.7 final integrated closure

- [ ] Final diff, scope-drift, dependency, secret/content/path และ static mutation
  review ไม่มี blocker ค่ะ
- [ ] No unresolved P0/P1, data-loss, token-leak, silent-overwrite หรือ ambiguous
  deletion finding ค่ะ
- [ ] Plan, acceptance, Results, Project Plan, Session Handoff, README และ Changelog
  ใช้สถานะ/checkpoint เดียวกันค่ะ
- [ ] AI worker usage ledger แยก reported tokens, context, quota และ credits ตาม
  measurement contract โดยไม่สร้างตัวเลขเทียมค่ะ
- [ ] Exact candidate HEAD ผ่าน deterministic, platform, live, CI และ security gates ค่ะ
- [ ] R3 PR reviewed, Ready, checks green และ approved merge complete ค่ะ
- [ ] Post-merge verification ผ่านบน `main` ค่ะ
- [ ] R4 ยังคง Locked planned จนคุณโออนุมัติ transition แยกต่างหากค่ะ

## Evidence ownership

- Direction และ locked scope อยู่ที่ [../../PROJECT_PLAN.md](../../PROJECT_PLAN.md) ค่ะ
- R3 implementation contract อยู่ที่ [R3_PLAN.md](R3_PLAN.md) ค่ะ
- AI usage/efficiency contract อยู่ที่ [R3_USAGE.md](R3_USAGE.md) ค่ะ
- Runtime results ต้องเพิ่มใน [RESULTS.md](RESULTS.md) หลัง R3 activation ค่ะ
- Operational checkpoint อยู่ที่ [../../SESSION_HANDOFF.md](../../SESSION_HANDOFF.md) ค่ะ
