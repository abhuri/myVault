# R3 — Safe Conflict Core Plan

Owner: Sunday ค่ะ

Planning status: `R3.2 COMPLETE — GATE 2 PASSED — SOURCE 6d82b77` ค่ะ

R2 implementation ถูก merge เข้า `main` ผ่าน PR #27 ที่ `94db388` และ
post-merge Quality run `29429364407` ผ่านแล้วค่ะ R2 complete ตาม locked
milestone scope ค่ะ R2 documentation closure, R3 planning pack และ Option A
contracts ถูก merge ผ่าน PR #28 ที่ `main@eb6709c` หลัง Quality run
`29461969032` ผ่านบน exact source head `f120679` ค่ะ R3.0 closure ถูก merge
ผ่าน PR #29 ที่ canonical
`main@9a30ad9763b8a9503484f2a35e559b1c7ee800b6` หลัง run `29464396485`
ผ่านทั้ง `quality` และ `android-compile` ค่ะ

คุณโอให้ explicit `Approve R3 transition` เมื่อ 2026-07-16 ค่ะ Gate 0 จึงผ่าน
บน canonical checkpoint ข้างต้นค่ะ คุณโออนุมัติ execute เฉพาะ R3.1 Step 1
แบบ documentation-only contract/schema freeze ค่ะ Approval นี้ไม่ครอบคลุม
Step 2, Rust/SQL implementation, worker/agy run, commit, push, PR, merge,
live Drive mutation หรือ external action ค่ะ

ภายหลังคุณโออนุมัติ Step 2 แบบ read-only inventory พร้อม agy ที่มี bounded scope
และอนุมัติ Sol change-control findings A/B เมื่อ 2026-07-16 ค่ะ Approval ล่าสุดนี้
ยังไม่ครอบคลุม Step 3 source write หรือ external action ค่ะ

คุณโออนุมัติ R3.1 Step 3, Step 4, Sol change-control สำหรับ `outcome_code` และ
Step 5 ตามลำดับค่ะ Step 5 ใช้ Terra High implementation ภายใต้ frozen contract
และไม่มี Sol review ใหม่ในระหว่างงานค่ะ Sol High ถูกสงวนไว้สำหรับ audit หลัง
bounded implementation ค่ะ

## 1. Outcome

R3 Safe Conflict Core ทำให้สองอุปกรณ์สังเกตและ classify การแก้เนื้อหา, rename,
move และ Trash พร้อมกันโดยไม่เกิด silent overwrite, ambiguous remote deletion
หรือ automatic conflict deletion ค่ะ Safe merge และ preserve-both materialize
locally ได้ผ่าน guarded capability ค่ะ Intent ที่ต้องแก้ existing Drive item ต้อง
หยุดที่ `NeedsReconcile` พร้อมหลักฐานค่ะ

## 2. Locked safety boundary

- ใช้เฉพาะ disposable local Vaults, approved test account และ exact allowlisted
  disposable Drive root จนกว่า acceptance gate จะอนุญาตอย่างอื่นค่ะ
- R3 Safe Conflict Core ห้ามส่ง existing Drive item content update, rename, move
  หรือ `trashed=true` ค่ะ Intent เหล่านี้ต้องใช้ exact identity evidence และจบที่
  `NeedsReconcile` ค่ะ
- Remote surface จำกัดที่ R1/R2 read-only reconciliation และ proven create-only
  transfer contract เท่านั้นค่ะ ห้ามมี permanent-delete, permission mutation
  หรือ generic request capability ค่ะ
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
- Provider-safe existing-item content update, rename, move และ remote Trash อยู่ใน
  gate แยกนอก R3 Safe Conflict Core ค่ะ ห้ามเปิด gate ด้วย preflight +
  post-verification แทน atomic provider precondition ค่ะ

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
       -> R3.3 Remote Mutation Block Enforcement
       -> R3.4 Guarded Local Mutation Capability Proof
            \____________________  ____________________/
                                 \/
                  R3.5 Prerequisites and Safe Conflict Core Orchestration
                                 |
                      R3.4 Guarded Local Mutation Completion Gate
                                 |
                    R3.6 Two-device Fault and Platform Acceptance
                                 |
                    R3.7 Exact-head Closure

RM0 Provider-safe Remote Mutation Gate
  (separate change-control lane; not a dependency of R3 Safe Conflict Core)
```

R3.2, R3.3 block-enforcement และ R3.4 capability **proof** ทำคู่ขนานได้หลัง R3.1
contract freeze ค่ะ R3.4 executable completion ถูก defer จน R3.5 prerequisite
ผ่านค่ะ R3.5 เป็นต้นไปหยุด parallel source edits และให้ main integrator เป็นเจ้าของ
integration ค่ะ RM0 ห้ามส่ง diff เข้าสู่ R3 โดยไม่มี official provider evidence,
Sol High review และ approved change-control ใหม่ค่ะ

## 6. R3.x phases

### R3.0 — Activation and Safety Contract Freeze

Outcome คือมี canonical source checkpoint เดียวและทุก conflict cell มีผลลัพธ์
ที่ implementation ไม่ต้องตีความเองค่ะ

[R3_CONTRACTS.md](R3_CONTRACTS.md) เป็น canonical R3.0 safety artifact ค่ะ Sol
High review freeze fail-closed conflict outcomes, merge fallback, conflict-copy
identity และ provider limitation record แล้วค่ะ คุณโออนุมัติ Option A
change-control เมื่อ 2026-07-16 ให้ R3 ส่งมอบ Safe Conflict Core และแยก
Provider-safe Remote Mutation Gate ออกจาก dependency หลักค่ะ Existing-item Drive
mutations จึงคง `BLOCKED` โดยไม่ block R3 Safe Conflict Core ค่ะ

งานหลักมีดังนี้ค่ะ

- นำ R2 documentation closure `f7a0d7c` และ R3 planning pack ผ่าน review,
  CI, PR และ merge เข้า `main` หรือสร้าง equivalent canonical checkpoint ค่ะ
- Freeze conflict matrix สำหรับ local-only, remote-only, both-changed,
  delete/edit, rename/edit, move collision, duplicate path และ offline replay ค่ะ
- Freeze text/binary policy, Markdown merge safety, invalid encoding, LF/CRLF,
  frontmatter preservation และ bounded input limits ค่ะ
- Freeze conflict-copy naming, Unicode normalization, case folding, collision
  suffix, operation identity และ rerun idempotency ค่ะ
- Freeze mutation allowlist ให้ block existing-item Drive mutations, จำกัด
  supported topology, บันทึก provider precondition limit, post-verification และ
  unknown-outcome taxonomy ค่ะ
- ตรวจ Google Drive mutation semantics จาก official provider documentation ก่อน
  code และบันทึกข้อจำกัดที่ไม่มี server-enforced precondition ตามจริงค่ะ
- ขอ explicit approval จากคุณโอเพื่อ transition เข้า R3 ค่ะ

R3.0 exit ผ่านแล้วค่ะ Option A scope/dependency, conflict matrix, mutation block,
fixture/privacy bounds และ unknown-outcome taxonomy ถูก freeze โดยไม่มี
last-write-wins/permanent delete และอยู่บน canonical `main@eb6709c` ค่ะ Gate 0
activation เหลือ explicit transition approval เท่านั้นค่ะ

Preflight revision check + post-verification ห้ามถูกนับเป็น atomic stale-write
protection ค่ะ การแก้ provider limitation ด้วย generic Drive request, blind retry
หรือ permanent delete เป็น scope/safety violation ค่ะ

Owner คือ main integrator เพียงคนเดียวค่ะ

### R3.1 — Durable Mutation and Conflict Evidence

Outcome คือ mutation ทุกชนิด restart ได้โดยไม่เดาผลลัพธ์และ cursor ไม่ล้ำ side
effect ค่ะ

Step 1 schema/durable-state contract ถูก freeze ใน
[R3_1_DURABLE_EVIDENCE_CONTRACT.md](R3_1_DURABLE_EVIDENCE_CONTRACT.md) ค่ะ
Target schema คือ v4 ค่ะ

Step 2 inventory และ Sol change-control findings A/B complete เมื่อ 2026-07-16
ค่ะ Legacy R2 transfer timestamp guard ถูกจำกัดเป็น reject-only compatibility
guard และห้ามเป็น R3 correctness proof ค่ะ Existing v3 `move`/`trash` queue rows
เป็น dormant legacy records ที่ต้อง preserve แต่ห้าม backfill/execute เป็น R3
intent ค่ะ R3.3 ยังคงเป็น owner ของ claim-path block enforcement ค่ะ Step 3
transactional migration และ bounded tests complete แล้วค่ะ Immutable-intent/state
API, evidence persistence และ R3 restart recovery complete แล้วค่ะ Step 5 เพิ่ม
typed R3 change-batch registration, exact post-verify evidence/event binding,
legacy API exclusion, atomic cursor commit และ SQLite fault coverage แล้วค่ะ
Step 5 ไม่แตะ provider, UI, conflict classifier หรือ R3.2/R3.3 scope ค่ะ Step 6
ผ่าน focused migration/state/cursor/fault tests, strict format/Clippy, full engine
and transfer suites และ static schema/scope-drift audit แล้วค่ะ Closure audit เพิ่ม
canonical fingerprints, destination-path binding, immutable conflict-envelope API และ
exact cursor event equality ค่ะ Gate 1 local evidence ครบตาม package
[R3_1_STEP6_EVIDENCE.md](R3_1_STEP6_EVIDENCE.md) ค่ะ Option A ยังคง reject retry
transitions และจบที่ `NeedsReconcile` จนกว่า executor ที่ผ่าน approval จะมี exact
revalidation proof ค่ะ

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

R3.2 implementation ปิด pure classifier C01–C34, bounded Markdown merge,
versioned conflict identity/naming, sealed replay/semantic proofs, deterministic
materialization และ R3.1 intent/evidence integration แล้วค่ะ Full engine suite ผ่าน
113 tests พร้อม strict Clippy/Rustfmt/diff check ค่ะ Terra และ Sol audit findings
ถูกแก้ครบค่ะ Exact conflict-copy reuse คง `NeedsReconcile` จน R3.5 มี durable
verification event/dependency ที่พิสูจน์ cursor advancement ได้ค่ะ

### R3.3 — Remote Mutation Block Enforcement

Outcome คือ Safe Conflict Core พิสูจน์ได้ว่าไม่มี production path ส่ง existing
Drive item content update, rename, move หรือ Trash และ intent ทุกชนิดรักษา
หลักฐานก่อนจบที่ `NeedsReconcile` ค่ะ

**Status: complete** ที่ `main@538fb72d132b2b318298140f24780d65d01217a0` ค่ะ
Exact-head CI run `29494622309` ผ่านทั้ง `quality` และ `android-compile` ค่ะ

งานหลักมีดังนี้ค่ะ

- แยก typed `RemoteMutationBlocked` จาก read-only reconciliation และ R2
  create-only transfer capability ค่ะ
- Persist exact account/root/file/parent/base/revision evidence โดยไม่ส่ง
  existing-item mutation request ค่ะ
- Remote observation ของ content/name/parent/trashed state เข้า classifier ได้
  แต่ห้ามสร้าง `files.update` side effect ค่ะ
- Static production-surface audit ต้องพิสูจน์ว่าไม่มี existing-item update,
  generic request, HTTP `DELETE`, permission mutation หรือ OAuth broadening ค่ะ
- Shortcut, Google-native ambiguity, multiple parents, outside-root ancestry,
  malformed metadata, redirect และ origin change ยังคง fail closed ค่ะ
- Provider-safe research อยู่ใน RM0 แยกต่างหากและห้ามสร้าง source diff ใน R3 ค่ะ

Exit gate คือ blocked intent durable/idempotent, restart-safe, cursor-withheld และ
คืน `NeedsReconcile` โดยไม่มี remote existing-item side effect ค่ะ

Test gate คือ captured-request negative assertions, wrong account/root/parent,
stale revision/base, repeated Changes page, restart, static no-update/no-DELETE
audit และ proof ว่า R1/R2 create-only surface ไม่ถูก broaden ค่ะ

Owner คือ Drive/block-enforcement lane ภายใต้ `crates/myvault-drive/**` ค่ะ

### R3.4 — Guarded Local Mutation Capability

Outcome คือ Desktop และ Android ใช้ typed result contract เดียวกันแม้ durability
guarantees ต่างกันค่ะ

**Status: open / controlled Option 1 proof-only execution** ค่ะ R3.5 prerequisite
candidate `4f0ba27711ea26f0a38b7dcfcc7d94ae1f439b40` ผ่าน bounded Sol audit และ CI บน Draft PR
[#30](https://github.com/abhuri/myVault/pull/30) แล้ว แต่ยังไม่ merged เข้า `main` ค่ะ
คุณโอเลือก Option 1 และอนุมัติ controlled plan เมื่อ 2026-07-18 ค่ะ Phase B เปิด
เฉพาะ truth correction, Gate 5 inventory และ proof-only Android provider identity/
allowlist layer ค่ะ ห้ามเพิ่มหรือเรียก mutation primitive และไม่มี R3.4 item ใด complete ค่ะ
Desktop ยังไม่มี durable exact source identity, atomic/no-replace replacement,
final-outcome proof หรือ durable watcher/replay echo suppression ค่ะ Android SAF ยัง
ไม่มี provable held destination-parent identity, complete collision set, atomic
no-replace publication หรือ final outcome ค่ะ Android unavailable → `NeedsReconcile`
เป็น fail-closed capability outcome แต่ไม่ใช่ Gate 4 closure ค่ะ

**Proof-only result:** Terra High เพิ่ม exact provider/root attestation transport และ
empty shipped allowlist ใน `tauri-plugin-vault-saf` โดยไม่มี mutation primitive ค่ะ
Rust 12 tests, strict Clippy/Rustfmt, Android aarch64 check และ Kotlin generated-host
unit task ผ่านค่ะ Aggregate `pnpm quality:r2:offline` ผ่านด้วยค่ะ Rust reject
`eligible=true` จาก mobile bridge อย่างอิสระค่ะ ไม่มี
provider member ผ่าน no-replace/final-outcome proof จึง `STOP_ADAPTER` และ Gate 4 ยัง open ค่ะ

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

**Completion sequencing:** Proof-only provider identity/allowlist layer ทำได้ก่อน merge
โดยห้าม mutation ค่ะ R3.5 candidate ต้องถูก merge/integrate เป็น baseline ที่อนุมัติ
ก่อนเปิด adapter ค่ะ Sol High ต้อง freeze Option 1 contract ให้จำกัดเฉพาะ provider
allowlist และพิสูจน์ held root/document/parent identity, complete collision recheck,
no-replace/final outcome และ unknown-outcome handling ก่อน Terra High ทำ mutation
implementation ค่ะ Generic SAF, unsupported provider หรือ outcome ที่พิสูจน์ไม่ได้
ต้องคืน `WriteOutcomeUnknown` หรือ `NeedsReconcile` และห้าม advance cursor ค่ะ
R3.4 completion จะกลับมาเป็น bounded Desktop/Android platform matrix หลังหลักฐานนี้
ผ่านเท่านั้นค่ะ

### R3.5 — Safe Conflict Core Orchestration

Outcome คือ remote observation, local observation, classify, mutation, merge,
conflict copy, base publication และ cursor commit เป็น durable state machine เดียวค่ะ

**Status: prerequisite candidate green / Gate 5 open** ค่ะ Candidate `4f0ba27`
เพิ่ม schema v6, durable exact local identity, Sync journal, final-outcome classifier
และ echo/replay substrate พร้อม exact-head CI ค่ะ Candidate ไม่มี production platform
verifier/adapter call site และ checklist Gate 5 ยัง 0/9 ค่ะ การ merge candidate อย่างเดียว
จึงไม่ใช่ R3.5 completion ค่ะ

Production call-site inventory ยืนยันว่า local observation ingestion, remote event to
guarded materialization, ordered execution runner, R3.5 per-Vault worker, taxonomy binding,
echo/replay consumer, convergence adapter, restart runner และ redacted R3.5 status projection
ยังไม่มี evidence ที่ check ได้ค่ะ

**Entry prerequisite gate:** R3.5 ต้องเริ่มด้วย Sol High change-control ของ durable
exact local identity, journal/recovery extension, unknown final outcome และ
watcher/SAF echo-to-replay semantics ค่ะ หลัง disposition และ explicit user approval
ที่จำกัด exact source/test scope แล้ว Terra High ทำได้เฉพาะ bounded implementation/test
ตาม contract ที่อนุมัติค่ะ Documentation closeout approval อย่างเดียวไม่เปิด source
write ค่ะ ห้ามใช้ R3.5 เพื่อผ่อน `legacy_v3`/cursor gates หรือเปิด remote
existing-item mutation ค่ะ

งานหลักมีดังนี้ค่ะ

- ต่อ local rename/move/Trash observation เข้ากับ durable queue ค่ะ
- เปลี่ยน remote move/removal/rename branches ที่ R2 fail closed ให้ผ่าน
  classifier และ guarded local materialization หรือ `NeedsReconcile` โดยห้ามส่ง
  existing-item Drive mutation ค่ะ
- ล็อกลำดับ stage/read → classify → durable intent → side effect → post-verify
  → base publish → completion → cursor commit ค่ะ
- ใช้ worker หนึ่งตัวต่อ Vault และไม่ถือ app/store lock ข้าม network หรือ large
  I/O ค่ะ
- Reuse R2 offline, auth, retry, redaction และ stale-session taxonomy ค่ะ
- Duplicate retry, watcher echo และ repeated Changes page ห้ามสร้าง side effect
  หรือ conflict copy ซ้ำค่ะ
- Merge/conflict result ต้อง converge ผ่าน guarded local replacement,
  preserve-both/conflict-copy publication หรือ `NeedsReconcile` ค่ะ Remote
  create-only conflict-copy upload ใช้ได้เฉพาะเมื่อ reuse R2 exact create-only
  contract โดยไม่ mutate existing item ค่ะ
- Expose เฉพาะ redacted status ที่จำเป็นต่อ R3 acceptance ค่ะ Full recovery UI
  อยู่ R4 ค่ะ
- ส่ง durable proof ที่ผ่านแล้วกลับไปยัง R3.4 completion matrix ค่ะ R3.5 orchestration
  เองไม่ถือว่า Gate 4 complete และต้องไม่มี local side effect ก่อน platform capability
  ที่เกี่ยวข้องพิสูจน์ precondition/final outcome ได้ค่ะ

Exit gate คือ restart ทุก boundary ไม่ทำให้ duplicate side effect, lost conflict
copy หรือ cursor drift ค่ะ

Test gate คือ integration matrix ครบ local-only, remote-only, both-changed,
delete/edit, rename/edit, move collision, duplicate path, offline replay,
auth expiry และ cursor withholding ค่ะ

Owner คือ main integrator ที่ดูแล `crates/myvault-transfer/**`, `apps/tauri/**`,
shared manifests, lockfiles และ workflows ค่ะ

### R3.6 — Two-device Fault and Platform Acceptance

Outcome คือพิสูจน์ Safe Conflict Core บน deterministic two-device setup และ
ยืนยันว่าแอปไม่ส่ง existing-item Drive mutation ค่ะ

งานหลักมีดังนี้ค่ะ

- รัน macOS two-device fixture สำหรับ observation/classification ของ rename,
  move, Trash, safe merge, preserve both และ conflict copy ค่ะ
- Restart ระหว่าง blocked remote intent, guarded local mutation, merge publication, conflict-copy
  publication, base publication และ pre-cursor commit ค่ะ
- รัน offline edits สองฝั่งแล้ว replay ตาม conflict matrix ค่ะ
- ยืนยัน recursive byte/hash manifest และ remote identity หลังทุก scenario ค่ะ
- รัน Android API 36 emulator สำหรับ guarded local mutation/conflict path ค่ะ
- รัน Ubuntu/Windows compile, test และ packaging gates โดยไม่อ้าง native UAT ค่ะ
- ตรวจ static/runtime ว่าไม่มี existing-item `files.update`, remote Trash,
  permanent delete หรือ auto-delete conflict copy ค่ะ Live provider regression
  จำกัดที่ approved R1/R2 read/create-only contract ค่ะ

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
| content changed | unchanged | `NeedsReconcile` ด้วย reason `RemoteMutationBlocked` ค่ะ ห้าม remote existing-item update ใน R3 Safe Conflict Core ค่ะ |
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
| Main Sunday integrator | ใช้ phase-matrix default ค่ะ Terra Medium/High สำหรับ bounded execution, Sol High สำหรับ high-risk judgment และ Extra High เฉพาะ planned escalation ค่ะ | Contract freeze, conflict semantics, integration, security และ final gate ค่ะ |
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

### Phase model quick reference

[R3_USAGE.md section 7](R3_USAGE.md#7-phase-model-routing-and-session-bootstrap)
เป็น canonical source ของ routing และ session declaration ค่ะ ตารางนี้เป็น
quick-reference เท่านั้นค่ะ

| Phase | Main Sunday route | `agy` route |
|---|---|---|
| `R3.0` | Terra Medium → Sol High ที่ Gate 0 ค่ะ | Gemini 3.5 Flash (Medium) ค่ะ |
| `R3.1` | Sol High → Terra High หลัง contract freeze ค่ะ | Gemini 3.5 Flash (Medium) ค่ะ |
| `R3.2` | Terra High → Sol High ที่ conflict-policy gate ค่ะ | Gemini 3.5 Flash (Medium/High) ค่ะ |
| `R3.3` | Sol High ค่ะ | Gemini 3.5 Flash (High) ค่ะ |
| `R3.4` | Terra High → Sol High ที่ SAF safety gate ค่ะ | Gemini 3.5 Flash (Medium) ค่ะ |
| `R3.5` | Sol High → Extra High เฉพาะ final adversarial/P0/P1 ค่ะ | Gemini 3.5 Flash (Medium) ค่ะ |
| `R3.6` | Terra Medium → Sol High เมื่อมี unexplained safety failure ค่ะ | Gemini 3.5 Flash (Low/Medium) ค่ะ |
| `R3.7` | Sol High ค่ะ | Gemini 3.5 Flash (Medium) ค่ะ |

ทุก R3 session ต้องประกาศ phase, Main model/effort, gate model, `agy` tier,
เหตุผล, allowed scope และ approval state ก่อน source write, worker spawn หรือ
`agy` run ค่ะ

### Parallel ownership

หลัง R3.1 freeze ใช้สูงสุดหนึ่ง main integrator และสาม workers ค่ะ

- Worker A เป็น pure conflict lane และแตะเฉพาะ owned modules/fixtures ค่ะ
- Worker B เป็น Drive observation/mutation-block lane และแตะเฉพาะ
  `crates/myvault-drive/**` ค่ะ ห้ามทำ RM0 provider-mutation source work ค่ะ
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
- [OpenAI latest GPT-5.6 model guidance](https://developers.openai.com/api/docs/guides/latest-model.md) ค่ะ
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
- RM0 proposal อ้าง provider correctness precondition ที่ official evidence
  พิสูจน์ไม่ได้ค่ะ
- ต้องเพิ่ม permanent-delete, permission mutation หรือ broaden OAuth scope/user ค่ะ
- พบ confirmed P0/P1 data-loss หรือ security incident ค่ะ
- ต้องเปลี่ยน locked R3 scope, order, exit gate หรือ Personal First Release ค่ะ
- AI worker ต้องเข้าถึงข้อมูลหรือทำ action เกิน usage/security contract ค่ะ

## 12. Planning range

R3 Safe Conflict Core range ยังคง 2–3 focused engineering weeks ค่ะ Risk-adjusted
plan ใช้ประมาณ 3 focused weeks และ buffer ถึงสัปดาห์ที่ 4 หาก Android SAF หรือ
fault matrix เปิด unknown ใหม่ค่ะ RM0 Provider-safe Remote Mutation Gate ไม่มี
estimate และไม่อยู่ใน range นี้จนผ่าน official-evidence/change-control gate ค่ะ
Planning range ไม่ใช่ deadline lock และห้ามใช้ buffer เป็นเหตุผลขยาย scope ค่ะ
