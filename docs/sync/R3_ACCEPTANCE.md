# R3 — Safe Conflict Core Acceptance

Owner: Sunday ค่ะ

Current status: `R3.1 COMPLETE — GATE 1 PASSED — IMPLEMENTATION main@c774324` ค่ะ

R3 ถือว่า complete เมื่อทุก applicable checkbox ด้านล่างมี evidence จาก exact
candidate HEAD เดียวค่ะ Mock, compile, emulator, native runtime และ live evidence
ต้องถูกแยกชื่ออย่างตรงไปตรงมาค่ะ Gate 0 ผ่านแล้วค่ะ R3.1 action แต่ละช่วงยัง
ต้องอยู่ภายใต้ approval และ stop conditions ที่ประกาศไว้ค่ะ

## Gate 0 — R3.0 activation and contract freeze

- [x] R2 source candidate ถูก merge ผ่าน PR #27 ที่ `94db388` ค่ะ
- [x] Post-merge Quality run `29429364407` ผ่านบน `main@94db388` ค่ะ
- [x] R2 documentation closure, R3 planning pack และ Option A contracts ถูก merge
  ผ่าน PR #28 ที่ canonical `main@eb6709c` ค่ะ Quality run `29461969032` ผ่านทั้ง
  `quality` และ `android-compile` บน exact source head `f120679` ก่อน merge ค่ะ
- [x] `R3_PLAN.md`, acceptance, usage contract, Project Plan, README และ Session
  Handoff ผ่าน cross-document review ค่ะ
- [x] Conflict matrix, mutation allowlist, folder topology, merge policy,
  conflict-copy identity/naming และ unknown-outcome taxonomy ถูก freeze ค่ะ
- [x] Official Drive mutation semantics และ precondition limits ถูกบันทึกใน
  [R3_CONTRACTS.md](R3_CONTRACTS.md) ค่ะ
- [x] Disposable account/root และ two-device fixture schema, aliases, redaction
  และ privacy bounds ถูก freeze โดยไม่เปิดเผย credential หรือ personal path ค่ะ
  Exact runtime fingerprints ต้องอนุมัติก่อน R3.6 live regression ค่ะ
- [x] คุณโออนุมัติ transition เข้า R3 อย่างชัดเจนด้วยข้อความ
  `Approve R3 transition` เมื่อ 2026-07-16 บน canonical
  `main@9a30ad9763b8a9503484f2a35e559b1c7ee800b6` ค่ะ

คุณโออนุมัติ Option A change-control เมื่อ 2026-07-16 ค่ะ R3 scope จึง freeze เป็น
Safe Conflict Core และแยก Provider-safe Remote Mutation Gate ออกจาก dependency
หลักค่ะ Existing Drive item content update, rename, move และ Trash ยังคง blocked
และ intent จบที่ `NeedsReconcile` ค่ะ R3.0 content freeze และ canonicalization
complete แล้วค่ะ Gate 0 activation complete บน canonical checkpoint ข้างต้นค่ะ

## Gate 1 — R3.1 durable mutation and conflict evidence

Step 1 contract/schema decisions ถูก freeze ใน
[R3_1_DURABLE_EVIDENCE_CONTRACT.md](R3_1_DURABLE_EVIDENCE_CONTRACT.md) ค่ะ
Checkbox implementation/evidence ด้านล่างผ่านบน implementation commit
`main@c774324` และ local validation candidate เดียวกันค่ะ CI evidence อยู่ภายใต้
normal post-push workflow และไม่เปลี่ยน Gate 1 local closure ค่ะ

Step 2 inventory ณ เวลานั้นยืนยันว่า production source ยังเป็น schema v3 ค่ะ Sol
change-control A/B จำกัด legacy transfer timestamp เป็น reject-only compatibility
guard ที่ไม่ใช่ R3 proof และจำกัด v3 `move`/`trash` queue rows เป็น dormant legacy
records ที่ preserve ได้แต่ห้าม backfill/execute เป็น R3 intent ค่ะ คำตัดสินนี้ไม่
ทำให้ Gate 1 checkbox ใดผ่าน และ R3.3 ยังเป็น owner ของ claim-path block
enforcement ค่ะ

Step 3 สร้าง/validate schema v4, immutable-record triggers และ transactional
v3-to-v4 migration แล้วค่ะ Migration preserve legacy queue/transfer/batch facts,
map `applying` batch row เป็น `needs_reconcile`, ห้าม fabricate R3 evidence และ
gate cursor เมื่อพบ `legacy_v3` dependency ค่ะ

Step 4 เพิ่ม immutable intent registration, state-version transition, append-only
event/evidence persistence, `outcome_code` ที่ bind กับ evidence/state/event,
remote-existing blocked registration และ restart recovery ของ `running` mutation
ให้จบ `NeedsReconcile` พร้อม event/evidence ค่ะ Step 5 เพิ่ม typed dependency
registration ที่ map immutable operation kind แบบ fail closed, exact
`post_verify`/`VerifiedApplied` evidence-event bind, legacy API exclusion และ atomic
cursor update/delete ค่ะ Tests ครอบคลุม mixed dependency, preflight rejection,
restart boundary และ SQLite abort ก่อน evidence bind/cursor update ค่ะ Closure audit
เพิ่ม canonical engine fingerprints, destination-path post verification, immutable
conflict-envelope persistence/read API และ exact state/evidence/event cursor equality ค่ะ

Step 6 รัน focused migration/state/cursor/fault tests, strict format/Clippy, final full
`myvault-sync-engine` 61-test suite และ `myvault-transfer` 15-test compatibility
suite รวมถึง diff check แล้วค่ะ Static schema/durable-field/scope-drift audit ไม่พบ provider capability,
R3.2 classifier, UI หรือ local-materialization drift ค่ะ Evidence package อยู่ที่
[R3_1_STEP6_EVIDENCE.md](R3_1_STEP6_EVIDENCE.md) ค่ะ ผลนี้เป็น local dirty-tree
candidate evidence ค่ะ Sol audit ได้แก้ `VerifiedApplied` ให้ bind immutable intent
และ reject preflight completion แล้วค่ะ คุณโออนุมัติ Option A ให้ R3.1 reject
`VerifiedNotApplied`/`RetrySafe` transition และใช้ `NeedsReconcile` แทนจนกว่า
approved executor จะพิสูจน์ exact revalidation ได้ค่ะ Gate 1 local evidence ครบแล้ว
และไม่มี R3.2/provider scope drift ค่ะ

Post-push Quality run `29472405503` พบ stale Tauri integration expectation หนึ่งจุด
ค่ะ New incremental download ยังถูกบันทึกใน legacy transfer batch ดังนั้น exact local
bytes และ base publication อาจสำเร็จแต่ R3.1 ต้อง reject cursor advancement และคง
active batch ไว้จนมี typed intent/evidence reconciliation ค่ะ Baseline regression ต้อง
pin fail-closed behavior นี้โดยไม่ถอด `legacy_v3` guard ค่ะ การคืน operational cursor
advancement เป็นงาน typed orchestration ของ R3.5 และไม่ใช่ authority ของ R3.2 ค่ะ

- [x] Schema v3 migrate ไป schema รุ่น R3 แบบ transactional โดยรักษา binding,
  cursor, queue, history, transfer และ base evidence ค่ะ
- [x] Newer, negative, malformed, partial หรือ constraint-weakened schema ถูก
  preserve และ reject โดยไม่ repair อัตโนมัติค่ะ
- [x] Mutation evidence มี exact IDs/parents/paths, expected revisions, base ref,
  operation marker, durable phase, retry state และ redacted outcome ค่ะ
- [x] Conflict evidence มี immutable envelope/API, conflict-copy operation identity
  และ bounded explanation metadata โดยไม่ใช้ timestamp เป็น correctness input ค่ะ
- [x] SQLite ไม่มี credential, provider body, content body, ambient path หรือ
  bearer-like capability ค่ะ
- [x] Restart หลัง durable-intent boundary จบเป็น verified completion,
  retry-safe state หรือ `NeedsReconcile` ค่ะ
- [x] Cursor ถูกกั้นจน mutation, merge/conflict publication และ base publication
  commit ครบค่ะ
- [x] Exact operation retry idempotent และ mismatched ID reuse fail closed ค่ะ

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

## Gate 3 — R3.3 remote mutation block enforcement

- [ ] `RemoteMutationBlocked` แยกจาก read-only reconciliation และ R2 create-only
  transfer capability ค่ะ
- [ ] Existing-item content update, rename, move และ remote Trash intent เก็บ
  exact identity/base evidence แล้วจบที่ `NeedsReconcile` ค่ะ
- [ ] Remote content/name/parent/trashed observations เข้า classifier ได้โดยไม่ส่ง
  `files.update` request ค่ะ
- [ ] Restart และ repeated Changes page ไม่สร้าง existing-item side effect หรือ
  duplicate blocked intent ค่ะ
- [ ] Cursor ไม่ advance ข้าม unresolved blocked intent ค่ะ
- [ ] Shortcut, Google-native ambiguity, multiple parent, outside-root, malformed
  metadata, redirect และ origin change fail closed ค่ะ
- [ ] Captured requests และ static audit พิสูจน์ว่า production surface ไม่มี
  existing-item update, HTTP `DELETE`, permission mutation หรือ generic request ค่ะ
- [ ] R1/R2 read/create-only surface ไม่ถูก broaden และ provider-safe research
  ไม่มี source diff ปนใน Safe Conflict Core ค่ะ

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
- [ ] Remote rename/move/removal ผ่าน classifier และ guarded local materialization
  หรือ `NeedsReconcile` โดยไม่มี existing-item Drive mutation ค่ะ
- [ ] Execution order เป็น stage/read → classify → durable intent → side effect
  → post-verify → base publish → completion → cursor commit ค่ะ
- [ ] Worker หนึ่งตัวต่อ Vault และไม่ถือ app/store lock ข้าม network/large I/O ค่ะ
- [ ] Offline, auth, retry, stale session และ redaction reuse R2 taxonomy ค่ะ
- [ ] Duplicate retry, watcher echo และ repeated Changes page ไม่สร้าง duplicate
  mutation หรือ conflict copy ค่ะ
- [ ] Safe merge/conflict result converge ด้วย guarded local replacement,
  preserve-both/conflict-copy publication หรือ `NeedsReconcile` ค่ะ R2
  create-only upload ใช้ได้เฉพาะ contract เดิมและห้าม mutate existing item ค่ะ
- [ ] Restart ทุก persistent boundary ไม่ทำให้ lost conflict copy หรือ cursor drift ค่ะ
- [ ] Frontend เห็นเฉพาะ redacted minimum status และไม่มี R4 control-plane scope ค่ะ

## Gate 6 — R3.6 deterministic and live acceptance

- [ ] `quality:r3:offline` หรือ equivalent frozen aggregate ผ่าน frontend,
  Rustfmt, strict Clippy, unit, integration, migration, property, fault และ doc tests ค่ะ
- [ ] R1–R2 regression matrix ยังคงผ่านค่ะ
- [ ] Exact disposable account/root fingerprints และ local Vault A/B aliases
  ได้รับ approval ก่อน live regression โดยไม่มี credential/personal path ใน repo ค่ะ
- [ ] macOS disposable two-device journey ผ่าน observation/classification ของ
  rename, move, Trash, safe merge, preserve both, conflict copy และ recursive
  byte/hash manifest ค่ะ
- [ ] Restart ระหว่าง blocked remote intent, guarded local mutation, merge,
  conflict copy, base publish
  และ pre-cursor commit ผ่านค่ะ
- [ ] Offline two-sided edits replay ตาม matrix โดยไม่สูญข้อมูลค่ะ
- [ ] Static/runtime evidence ยืนยันว่าไม่มี existing-item `files.update`, remote
  Trash, permanent delete หรือ auto-delete conflict copy ค่ะ Live provider
  regression จำกัดที่ approved R1/R2 read/create-only contract ค่ะ
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
