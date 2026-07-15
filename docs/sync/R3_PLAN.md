# R3 — Remote Mutations and Conflict Safety Plan

Owner: Sunday ค่ะ

Planning status: `PREPARED — IMPLEMENTATION NOT ACTIVATED` ค่ะ

R2 implementation ถูก merge เข้า `main` ผ่าน PR #27 ที่ `94db388` และ
post-merge Quality run `29429364407` ผ่านแล้วค่ะ R2 complete ตาม locked
milestone scope ค่ะ เอกสารปิด R2 บน branch `codex/r2-closure` อยู่ที่
`f7a0d7c` และยังต้องเข้า canonical `main` พร้อม planning pack นี้หรือ diff
ที่สืบทอดเนื้อหาเดียวกันค่ะ

R3 source implementation ห้ามเริ่มจนกว่า R3.0 gate ผ่านและคุณโออนุมัติ
transition เข้า R3 อย่างชัดเจนค่ะ คุณโออนุมัติ commit, push และเปิด Draft PR
สำหรับ R2 closure/R3 planning documents เมื่อ 2026-07-15 ค่ะ Approval นี้ไม่
ครอบคลุม rename, move, Trash, conflict mutation, live Drive mutation, R3 source
implementation, PR merge หรือ R3 transition ค่ะ

## 1. Outcome

R3 ทำให้สองอุปกรณ์ rename, move, Trash และแก้เนื้อหาพร้อมกันได้โดยไม่เกิด
silent overwrite, ambiguous remote deletion หรือ automatic conflict deletion ค่ะ
ทุก side effect ต้องจบเป็น verified completion, verified not-applied และ
retry-safe, หรือ `NeedsReconcile` ที่รักษาหลักฐานเพียงพอให้แก้ต่อได้ค่ะ

## 2. Locked safety boundary

- ใช้เฉพาะ disposable local Vaults, approved test account และ exact allowlisted
  disposable Drive root จนกว่า acceptance gate จะอนุญาตอย่างอื่นค่ะ
- Remote mutation ใช้ exact account, root, file ID, source parent ID,
  destination parent ID และ expected revision เท่านั้นค่ะ
- Production API surface มีได้เฉพาะ exact-ID existing-content update ที่
  classifier พิสูจน์ว่า remote ยังตรง base, rename, move, guarded folder topology
  ที่ R3.0 อนุมัติ และตั้ง `trashed=true` ค่ะ ห้ามมี permanent-delete หรือ generic
  request capability ค่ะ
- Timestamp และ device label ใช้เพื่ออธิบาย conflict เท่านั้นค่ะ ห้ามใช้เลือก
  winner หรือพิสูจน์ correctness ค่ะ
- Markdown three-way merge ทำได้เมื่อ base/local/remote ชัดและพิสูจน์ได้ว่า
  edits ไม่ทับกันค่ะ กรณีอื่นต้องรักษาทั้งสองเวอร์ชันหรือหยุดที่
  `NeedsReconcile` ค่ะ
- Binary both-changed ต้องรักษาทั้งสองเวอร์ชันเสมอค่ะ
- Conflict copy ห้ามถูกลบอัตโนมัติและ exact retry ห้ามสร้างสำเนาซ้ำค่ะ
- `.obsidian/` และ `.trash/` ยังเป็น protected paths และไม่เข้า normal Sync
  state ค่ะ Vault-local Trash ใช้ capability และ evidence แยกจาก normal path ค่ะ
- Token, provider body, content body, resumable capability และ ambient Vault
  path ห้ามอยู่ใน WebView, SQLite, log, usage ledger หรือ serialized error ค่ะ

## 3. Non-goals

- Full Sync control-plane, user-driven conflict recovery UI และ diagnostics
  export เป็น R4 ค่ะ
- Complete local CRUD และ attachment UI journey เป็น R5 ค่ะ
- Physical Android acceptance และ full Windows/Ubuntu native UAT เป็น R7 ค่ะ
- Permanent delete, permissions mutation, Google-native file conversion,
  shortcuts, multi-user collaboration และ silent last-write-wins ไม่อยู่ใน R3 ค่ะ
- Generic folder Sync ห้ามเกิดจากการขยาย supporting folder topology ของ R3 ค่ะ

## 4. Foundation that R3 must reuse

- `myvault-sync-engine` มี durable queue, exact remote identity, change batch,
  cursor gating, completed-operation tombstone และ restart recovery แล้วค่ะ
- R2 เก็บ immutable base objects พร้อม local revision, remote revision และ
  content hash สำหรับ three-way classification แล้วค่ะ
- `myvault-drive` มี exact-root read, create-only upload และ verified download
  capability แล้วค่ะ Existing-object mutation ต้องเป็น capability ใหม่ที่แคบค่ะ
- `myvault-mutations` มี revision-checked Trash, restore, normal move และ
  case-only rename พร้อม recovery journal สำหรับ Desktop แล้วค่ะ
- Desktop/private-root และ Android SAF มี guarded staging/transfer primitives
  แล้วค่ะ R3 ต้องรายงาน weaker provider outcome ตามจริงแทนการอ้าง atomicity ค่ะ
- R2 retry, auth, offline, redaction และ single-worker contracts ต้องถูก reuse
  และห้ามสร้าง taxonomy คู่ขนานค่ะ

## 5. Dependency graph

```text
R3.0 Contract Freeze
  -> R3.1 Durable Mutation and Conflict Evidence
       -> R3.2 Pure Conflict Classification and Materialization
       -> R3.3 Exact-ID Drive Mutation Capability
       -> R3.4 Guarded Local Mutation Capability
            \____________________  ____________________/
                                 \/
                    R3.5 Mutation and Conflict Orchestration
                                 |
                    R3.6 Two-device Fault and Platform Acceptance
                                 |
                    R3.7 Exact-head Closure
```

R3.2, R3.3 และ R3.4 ทำคู่ขนานได้หลัง R3.1 contract freeze ค่ะ R3.5 เป็นต้นไป
หยุด parallel source edits และให้ main integrator เป็นเจ้าของ integration ค่ะ

## 6. R3.x phases

### R3.0 — Activation and Safety Contract Freeze

Outcome คือมี canonical source checkpoint เดียวและทุก conflict cell มีผลลัพธ์
ที่ implementation ไม่ต้องตีความเองค่ะ

งานหลักมีดังนี้ค่ะ

- นำ R2 documentation closure `f7a0d7c` และ R3 planning pack ผ่าน review,
  CI, PR และ merge เข้า `main` หรือสร้าง equivalent canonical checkpoint ค่ะ
- Freeze conflict matrix สำหรับ local-only, remote-only, both-changed,
  delete/edit, rename/edit, move collision, duplicate path และ offline replay ค่ะ
- Freeze text/binary policy, Markdown merge safety, invalid encoding, LF/CRLF,
  frontmatter preservation และ bounded input limits ค่ะ
- Freeze conflict-copy naming, Unicode normalization, case folding, collision
  suffix, operation identity และ rerun idempotency ค่ะ
- Freeze mutation allowlist, supported folder topology, provider precondition,
  existing-content update, post-verification และ unknown-outcome taxonomy ค่ะ
- ตรวจ Google Drive mutation semantics จาก official provider documentation ก่อน
  code และบันทึกข้อจำกัดที่ไม่มี server-enforced precondition ตามจริงค่ะ
- ขอ explicit approval จากคุณโอเพื่อ transition เข้า R3 ค่ะ

Exit gate คือ planning docs อยู่บน canonical checkpoint, scope ไม่มี
last-write-wins/permanent delete, conflict matrix ไม่มีช่องกำกวม และ approval
state ชัดเจนค่ะ

Owner คือ main integrator เพียงคนเดียวค่ะ

### R3.1 — Durable Mutation and Conflict Evidence

Outcome คือ mutation ทุกชนิด restart ได้โดยไม่เดาผลลัพธ์และ cursor ไม่ล้ำ side
effect ค่ะ

งานหลักมีดังนี้ค่ะ

- ทำ transactional migration จาก schema v3 ไป schema รุ่น R3 ที่ freeze แล้วค่ะ
- บันทึก exact file/parent IDs, source/destination paths, expected local/remote
  revisions, immutable base reference และ operation marker ค่ะ
- บันทึก immutable intent, durable phase, verification evidence, retry state,
  redacted outcome, conflict classification และ conflict-copy operation ID ค่ะ
- บันทึก bounded device/time explanation metadata โดยห้ามใช้เป็น correctness
  input ค่ะ
- ขยาย change batch ให้ mutation, merge publication, conflict-copy publication
  และ base publication เป็น cursor dependencies ค่ะ
- รักษา completed tombstone และ reject mismatched operation-ID reuse ค่ะ

Exit gate คือ crash หลัง durable-intent boundary ทุกจุดกลับมาเป็น verified
completion, retry-safe state หรือ `NeedsReconcile` ได้ค่ะ

Test gate คือ migration preservation, malformed/newer schema rejection, property
tests ของ state transition, restart recovery, ID collision และ cursor gating ค่ะ

Owner คือ state lane ภายใต้ `crates/myvault-sync-engine/**` ค่ะ

### R3.2 — Pure Conflict Classification and Materialization

Outcome คือ base/local/remote ถูกจำแนกเป็น deterministic typed plan โดยไม่ผูกกับ
Tauri, network หรือ filesystem provider ค่ะ

งานหลักมีดังนี้ค่ะ

- จำแนกจาก exact identity, revision, hash และ path lineage ค่ะ
- ทำ Markdown three-way merge เฉพาะเมื่อ base ชัดและ edits ไม่ทับกันค่ะ
- สร้าง conflict-copy plan เมื่อ edits ทับกัน, base ขาด, encoding ไม่ปลอดภัย,
  input เกิน bound หรือ classification กำกวมค่ะ
- รักษา binary both-changed และ edited bytes ใน delete-versus-edit ค่ะ
- เชื่อม rename-versus-edit ด้วย exact identity โดยไม่ match จากชื่อค่ะ
- ครอบคลุม move/rename cycle, case-only rename, destination collision,
  duplicate remote path และ parent-folder race ค่ะ
- วางแผน publish base reference ใหม่หลัง merge หรือ materialization ค่ะ
- ทำ naming ให้ portable, deterministic, collision-safe และ rerun-idempotent ค่ะ

Exit gate คือทุก matrix cell คืน typed safe plan หรือ `NeedsReconcile` โดยไม่มี
silent overwrite และไม่มี automatic conflict deletion ค่ะ

Test gate คือ golden/property fixtures สำหรับ Thai/Unicode, normalization,
case folding, empty file, LF/CRLF, frontmatter, overlapping/non-overlapping edits,
invalid/oversized Markdown และ binary pairs ค่ะ

Owner คือ pure conflict lane ค่ะ ห้ามสร้าง crate ใหม่จนกว่าจะพิสูจน์ trust boundary
หรือ independently testable lifecycle ที่จำเป็นค่ะ

### R3.3 — Exact-ID Drive Mutation Capability

Outcome คือ Drive mutation มีพื้นผิวแคบและพิสูจน์ exact identity/revision ก่อน
และหลัง side effect ค่ะ

งานหลักมีดังนี้ค่ะ

- เพิ่ม capability แยกจาก `ReadOnlyDrive` และ create-only `TransferDrive` สำหรับ
  guarded existing-content update, rename, move และ Trash ค่ะ
- Existing-content update ใช้ exact file ID, expected remote revision, base hash,
  intended hash/size และ bounded resumable/update protocol ที่ R3.0 freeze ค่ะ
- Content update ทำได้เมื่อ classifier พิสูจน์ว่า remote ยังตรง immutable base
  เท่านั้นค่ะ Remote ที่เปลี่ยนจาก base ต้องกลับเข้า conflict resolution ค่ะ
- Rename ใช้ exact file ID และ intended name ค่ะ
- Move ใช้ exact file ID, old parent ID และ new parent ID ค่ะ
- Trash ใช้ exact file ID และตั้ง Trash เท่านั้นค่ะ
- Re-verify account, bound root, ancestry, parent identity และ expected remote
  revision ก่อน mutation และ post-verify metadata หลัง mutation ค่ะ
- Lost response ระหว่าง content/metadata mutation ต้อง metadata/hash-reconcile
  ก่อน retry ค่ะ
- Reject shortcut, Google-native ambiguity, multiple parents, outside-root
  ancestry, malformed metadata, redirect และ origin change ค่ะ
- Supporting folder creation ทำได้เฉพาะ topology ที่ R3.0 อนุมัติและต้องใช้
  exact parent/operation marker ค่ะ

Exit gate คือทุก request จบเป็น verified applied, verified not-applied และ
retry-safe, หรือ `NeedsReconcile` ค่ะ Production surface ไม่มี HTTP `DELETE` ค่ะ

Test gate คือ captured-request allowlist, wrong account/root/parent, stale
revision/base hash, partial update, final response loss, hash mismatch,
401/403/404/410/429/5xx, timeout และ static no-DELETE audit ค่ะ

Owner คือ Drive lane ภายใต้ `crates/myvault-drive/**` ค่ะ

### R3.4 — Guarded Local Mutation Capability

Outcome คือ Desktop และ Android ใช้ typed result contract เดียวกันแม้ durability
guarantees ต่างกันค่ะ

งานหลักมีดังนี้ค่ะ

- Desktop reuse mutation service, recovery journal, revision guard, no-replace
  move และ case-only rename ค่ะ
- เพิ่ม Sync-owned rename, move, Vault-local Trash, guarded replacement และ
  conflict-copy publication adapters ค่ะ
- Recheck source, destination, parent และ revision ก่อน publication ค่ะ
- Android SAF เพิ่ม native rename/move/Trash/conflict-copy primitives ที่ bind
  exact held root และ document identity ค่ะ
- Provider ที่พิสูจน์ atomicity หรือ outcome ไม่ได้ต้องคืน
  `WriteOutcomeUnknown` หรือ `NeedsReconcile` ตามจริงค่ะ
- ห้ามเปิด binary in-place replacement เป็นทางลัดค่ะ
- Watcher/SAF echo เป็น hint และห้าม enqueue mutation ซ้ำค่ะ

Exit gate คือไม่มี silent replace, destination collision รักษาทั้งสองฝั่ง และ
restart reconcile journal/provider state ได้ค่ะ

Test gate คือ Desktop fault injection, source/destination/parent substitution,
case-only rename, Trash retry, Android fake-provider matrix, emulator restart,
unsupported provider และ unknown-outcome preservation ค่ะ

Owner คือ local-platform lane โดยแยก Desktop และ Android file ownership ค่ะ

### R3.5 — Mutation and Conflict Orchestration

Outcome คือ remote observation, local observation, classify, mutation, merge,
conflict copy, base publication และ cursor commit เป็น durable state machine เดียวค่ะ

งานหลักมีดังนี้ค่ะ

- ต่อ local rename/move/Trash observation เข้ากับ durable queue ค่ะ
- เปลี่ยน remote move/removal/rename branches ที่ R2 fail closed ให้ผ่าน
  classifier และ guarded mutation ค่ะ
- ล็อกลำดับ stage/read → classify → durable intent → side effect → post-verify
  → base publish → completion → cursor commit ค่ะ
- ใช้ worker หนึ่งตัวต่อ Vault และไม่ถือ app/store lock ข้าม network หรือ large
  I/O ค่ะ
- Reuse R2 offline, auth, retry, redaction และ stale-session taxonomy ค่ะ
- Duplicate retry, watcher echo และ repeated Changes page ห้ามสร้าง side effect
  หรือ conflict copy ซ้ำค่ะ
- Merge/conflict result ต้อง converge ผ่าน guarded local replacement, guarded
  exact-ID remote update หรือ create-only conflict-copy upload ตาม typed plan ค่ะ
- Expose เฉพาะ redacted status ที่จำเป็นต่อ R3 acceptance ค่ะ Full recovery UI
  อยู่ R4 ค่ะ

Exit gate คือ restart ทุก boundary ไม่ทำให้ duplicate side effect, lost conflict
copy หรือ cursor drift ค่ะ

Test gate คือ integration matrix ครบ local-only, remote-only, both-changed,
delete/edit, rename/edit, move collision, duplicate path, offline replay,
auth expiry และ cursor withholding ค่ะ

Owner คือ main integrator ที่ดูแล `crates/myvault-transfer/**`, `apps/tauri/**`,
shared manifests, lockfiles และ workflows ค่ะ

### R3.6 — Two-device Fault and Platform Acceptance

Outcome คือพิสูจน์ milestone outcome จริงบน disposable two-device setup ค่ะ

งานหลักมีดังนี้ค่ะ

- รัน macOS two-device fixture สำหรับ rename, move, Trash, safe merge และ
  conflict copy ค่ะ
- Restart ระหว่าง remote/local mutation, merge publication, conflict-copy
  publication, base publication และ pre-cursor commit ค่ะ
- รัน offline edits สองฝั่งแล้ว replay ตาม conflict matrix ค่ะ
- ยืนยัน recursive byte/hash manifest และ remote identity หลังทุก scenario ค่ะ
- รัน Android API 36 emulator สำหรับ platform-supported mutation/conflict path ค่ะ
- รัน Ubuntu/Windows compile, test และ packaging gates โดยไม่อ้าง native UAT ค่ะ
- ตรวจ exact-ID Trash, no permanent delete และ no auto-delete conflict copy ค่ะ

Exit gate คือ R3 acceptance Gate 0–6 ที่เกี่ยวข้องผ่านบน exact candidate HEAD ค่ะ
Gate 7 เป็น R3.7 PR/merge/post-merge closure ค่ะ

Owner คือ main integrator ค่ะ Test/log workers ช่วยรวบรวม evidence แบบ bounded
และ read-only ได้ค่ะ

### R3.7 — Exact-head Closure

Outcome คือ R3 มี candidate HEAD เดียว, evidence ครบและ handoff ไป R4 ไม่กำกวมค่ะ

งานหลักมีดังนี้ค่ะ

- Final diff, scope-drift, dependency, secret/content/path และ static mutation
  review ค่ะ
- Rerun required gates บน exact source HEAD เดียวค่ะ
- อัปเดต acceptance, Results, Project Plan, Session Handoff, README และ Changelog ค่ะ
- บันทึก deliberately untested behavior และคง physical Android ไว้ R7 ค่ะ
- PR, CI, merge และ post-merge verification ตาม approval ที่ได้รับค่ะ
- หลัง merge ให้ R4 Locked planned จนคุณโออนุมัติ transition ใหม่ค่ะ

Exit gate คือ Git, CI, live evidence และเอกสารชี้ checkpoint เดียวกันทั้งหมดค่ะ

Owner คือ main integrator เพียงคนเดียวค่ะ

## 7. Conflict matrix minimum coverage

| Local state | Remote state | Minimum safe result |
|---|---|---|
| unchanged | content changed | guarded local replace เมื่อ exact base ตรง หรือ `NeedsReconcile` ค่ะ |
| content changed | unchanged | guarded remote content update เมื่อ R3.0 อนุมัติ contract หรือ `NeedsReconcile` ค่ะ |
| content changed | content changed | safe three-way merge หรือ preserve both ค่ะ |
| delete/Trash | edit | preserve edited bytes และ conflict evidence ค่ะ |
| edit | delete/Trash | preserve edited bytes และ conflict evidence ค่ะ |
| rename | edit | bind ด้วย exact identity และรักษาทั้ง rename/content ค่ะ |
| move | move elsewhere | deterministic collision plan หรือ `NeedsReconcile` ค่ะ |
| destination created | move/rename arrives | no replace และ preserve both ค่ะ |
| binary changed | binary changed | preserve both เสมอค่ะ |
| parent moved/trashed | child edited/created | exact-lineage resolution หรือ `NeedsReconcile` ค่ะ |
| duplicate remote path | any local state | ห้ามเลือกจากชื่อ/time และหยุดอย่างปลอดภัยค่ะ |
| exact retry after restart | prior side effect unknown | reconcile identity/evidence ก่อน side effect ใหม่ค่ะ |

Matrix เต็มต้องอยู่ใน acceptance fixtures ก่อน R3.2 implementation ค่ะ

## 8. GPT and Antigravity staffing methodology

หลักการคือ main context เก็บ requirements, decisions และ final evidence เท่านั้นค่ะ
Exploration notes, test logs และ repetitive matrix work ต้องออกไปอยู่ใน bounded
workers แล้วคืนเฉพาะ summary พร้อม file/line references ค่ะ Subagents ช่วยลด
context pollution แต่ใช้ token รวมมากกว่า single-agent run จึงห้าม spawn งานที่
เล็กกว่าค่า coordination overhead ค่ะ

### OpenAI model routing

| Role | Recommended model/effort | Work class |
|---|---|---|
| Main Sunday integrator | GPT-5.6 Sol High หรือ Extra High ค่ะ | Contract freeze, conflict semantics, integration, security และ final gate ค่ะ |
| Safety/conflict reviewer | GPT-5.6 Sol High ค่ะ | Independent deep review, data-loss reasoning และ adversarial matrix ค่ะ |
| Bounded implementation worker | GPT-5.6 Terra Medium หรือ High ค่ะ | One owned lane, explicit files และ deterministic tests ค่ะ |
| Explorer/test/log worker | GPT-5.6 Terra Low/Medium ค่ะ | Read-heavy scan, test execution, triage และ concise evidence ค่ะ |
| Structured extraction worker | GPT-5.6 Luna Low/Medium ค่ะ | Tables, inventories, doc consistency และ repeated classification ค่ะ |

Current native `spawn_agent` surface ไม่เปิด per-call model selector ค่ะ เมื่อใช้
surface นี้ต้องบันทึก model เป็น `runtime-selected/unobservable` และห้ามอ้างว่า
pin model สำเร็จค่ะ หาก exact model และ per-run token accounting เป็น requirement
ให้ใช้ bounded `codex exec --ephemeral --json -m <model>` ใน temporary scoped
workspace แทนค่ะ Project-scoped custom agent files สามารถพิจารณาแยกต่างหากได้
แต่ไม่อยู่ใน planning-only diff นี้ค่ะ

### Antigravity model routing

| Role | Recommended model | Work class |
|---|---|---|
| Fast extraction | Gemini 3.5 Flash (Low) ค่ะ | Call-site lists, short summaries และ log grouping ค่ะ |
| Matrix/fixture analyst | Gemini 3.5 Flash (Medium) ค่ะ | Conflict expansion, test fixtures และ bounded first-pass review ค่ะ |
| Deep bounded second opinion | Gemini 3.5 Flash (High) ค่ะ | Narrow state-machine/security question หลัง contract freeze ค่ะ |

`agy` เป็น analyst/reviewer ไม่ใช่ final decision maker ค่ะ ห้ามให้ `agy` ทำ live
Drive mutation, access OAuth/keyring/personal Vault, merge/commit/push หรือแก้
shared integration files โดยตรงค่ะ ทุก run ใช้ fresh temporary workspace ที่มี
เฉพาะ allowlisted files, `--mode plan`, `--sandbox`, explicit model, output cap
และ direct-response contract ค่ะ

### Parallel ownership

หลัง R3.1 freeze ใช้สูงสุดหนึ่ง main integrator และสาม workers ค่ะ

- Worker A เป็น pure conflict lane และแตะเฉพาะ owned modules/fixtures ค่ะ
- Worker B เป็น Drive lane และแตะเฉพาะ `crates/myvault-drive/**` ค่ะ
- Worker C เป็น local-platform lane โดยแบ่ง Desktop/Android ownership ชัดค่ะ
- Main integrator ดูแล durable integration, `myvault-transfer`, Tauri,
  `sync_commands.rs`, manifests, lockfiles, workflows, evidence และ commits ค่ะ

ห้าม workers แก้ shared file เดียวกันพร้อมกันค่ะ ก่อน R3.5 ต้องหยุด parallel
source writes, รวม diff ทีละ lane, rerun component gate และตรวจ integration บน
head เดียวค่ะ

## 9. AI worker usage and efficiency contract

Usage ledger และ measurement contract อยู่ที่ [R3_USAGE.md](R3_USAGE.md) ค่ะ

- OpenAI interactive `/status` ใช้ดู session/context ค่ะ `/usage` ใช้ดู account
  activity/rate limits ค่ะ และ `codex exec --json` ให้ per-run input, cached input,
  output และ reasoning-output tokens ผ่าน `turn.completed.usage` ค่ะ
- Native spawned-agent token ต่อ child ไม่ปรากฏใน current tool surface ค่ะ จึง
  บันทึก worker count, role, wall time, scope, accepted work และ
  `runtime-selected/unobservable` แทนการสร้างตัวเลขเทียมค่ะ
- Antigravity `/usage` หรือ `/quota` แสดง quota จาก backend ค่ะ `/context` และ
  statusline JSON แสดง context-window tokens ค่ะ `/credits` แสดง overage credits ค่ะ
  สามค่านี้เป็นคนละหน่วยและห้ามนำมาแทนกันค่ะ
- `agy 1.1.2` ยังไม่มี supported exact token-per-headless-run output ค่ะ จึงใช้
  batch quota snapshots, context metrics เมื่อ validated และ accepted-work units ค่ะ
- Antigravity metrics collector ต้อง allowlist เฉพาะ counters ที่อนุมัติและทิ้ง
  email, cwd, workspace/project URI, plan tier, conversation ID กับ raw payload
  ก่อน persist ค่ะ Default คือ `not captured` จน synthetic privacy validation ผ่านค่ะ
- Prompt count, output bytes และ quota percentage ห้ามถูกเรียกว่า billable tokens ค่ะ
- Raw AI logs อยู่ภายนอก repository, permission จำกัดและ retention สั้นค่ะ

Efficiency decision ใช้ first-pass acceptance rate, accepted matrix cells/fixtures,
review minutes, failed runs และ quota delta มากกว่าจำนวนคำตอบค่ะ Worker ที่คืน
ผลซ้ำ, ไม่มี source references หรือ acceptance ต่ำกว่า 60% สอง batch ติดต่อกัน
ต้องถูกลดระดับงาน, เปลี่ยน prompt/model หรือหยุดใช้ใน lane นั้นค่ะ

## 10. Official methodology references

- [OpenAI Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents.md) ค่ะ
- [OpenAI Codex models](https://learn.chatgpt.com/docs/models.md) ค่ะ
- [OpenAI API model catalog](https://developers.openai.com/api/docs/models) ค่ะ
- [OpenAI Codex CLI commands](https://learn.chatgpt.com/docs/developer-commands.md?surface=cli) ค่ะ
- [OpenAI Codex non-interactive mode](https://learn.chatgpt.com/docs/non-interactive-mode.md) ค่ะ
- [Antigravity Model Quotas](https://antigravity.google/docs/cli/commands/usage) ค่ะ
- [Antigravity status line metrics](https://antigravity.google/docs/cli-statusline) ค่ะ
- [Antigravity plans and quota semantics](https://antigravity.google/docs/plans) ค่ะ
- [Antigravity SDK observability](https://antigravity.google/docs/sdk-overview) ค่ะ

## 11. Stop conditions

Sunday ต้องหยุดและขอ authority ใหม่เมื่อมีข้อใดข้อหนึ่งค่ะ

- จะเริ่ม R3 implementation ก่อน R3.0 gate/transition approval ค่ะ
- จะใช้ personal Vault, unallowlisted Drive data, credential หรือ token ค่ะ
- Provider semantics ไม่รองรับ correctness precondition ที่ frozen design พึ่งพาค่ะ
- ต้องเพิ่ม permanent-delete, permission mutation หรือ broaden OAuth scope/user ค่ะ
- พบ confirmed P0/P1 data-loss หรือ security incident ค่ะ
- ต้องเปลี่ยน locked R3 scope, order, exit gate หรือ Personal First Release ค่ะ
- AI worker ต้องเข้าถึงข้อมูลหรือทำ action เกิน usage/security contract ค่ะ

## 12. Planning range

Roadmap range เดิมคือ 2–3 focused engineering weeks ค่ะ Risk-adjusted plan ใช้
ประมาณ 3 focused weeks และ buffer ถึงสัปดาห์ที่ 4 หาก Android SAF, provider
precondition หรือ live fault matrix เปิด unknown ใหม่ค่ะ Planning range ไม่ใช่
deadline lock และห้ามใช้ buffer เป็นเหตุผลขยาย scope ค่ะ
