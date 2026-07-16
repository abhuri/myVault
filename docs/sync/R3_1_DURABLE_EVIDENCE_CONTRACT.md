# R3.1 — Durable Mutation and Conflict Evidence Contract

Owner: Sunday ค่ะ

Review route: GPT-5.6 Sol High ค่ะ

Status: `FROZEN — R3.1 COMPLETE — GATE 1 PASSED` ค่ะ

Published implementation: `main@c774324` ค่ะ

Canonical checkpoint: `main@9a30ad9763b8a9503484f2a35e559b1c7ee800b6` ค่ะ

Gate 0 activation: คุณโอให้ explicit `Approve R3 transition` และอนุมัติ
`R3.1 Step 1` เมื่อ 2026-07-16 Asia/Bangkok ค่ะ Approval นี้ครอบคลุม
documentation-only contract/schema freeze เท่านั้นค่ะ

เอกสารนี้ freeze durable truth สำหรับ R3.1 ก่อน bounded implementation ค่ะ
การเปลี่ยน field semantics, transition, migration disposition, cursor dependency
หรือ privacy boundary หลัง freeze ต้องกลับเข้า Sol High review ค่ะ

## 1. Outcome and scope

R3.1 ต้องทำให้ทุก mutation intent กลับมาหลัง restart โดยไม่เดาว่า side effect
เกิดขึ้นหรือไม่ค่ะ ทุก operation ต้องมี immutable identity, durable phase,
append-only evidence และ typed disposition ก่อน cursor จะ advance ค่ะ

ขอบเขตของ contract นี้มีดังนี้ค่ะ

- Freeze schema v4 semantics และ v3-to-v4 migration disposition ค่ะ
- Freeze immutable mutation intent, mutable state, append-only event และ
  verification evidence shape ค่ะ
- Freeze conflict evidence envelope โดยไม่ implement conflict classifier ค่ะ
- Freeze cursor dependency kinds และ exact evidence gate ค่ะ
- Freeze restart, retry, completed tombstone และ operation-ID collision rules ค่ะ
- Freeze privacy exclusions และ explanatory-metadata boundary ค่ะ

Contract นี้ไม่อนุญาต R3.2 classifier/merge implementation, R3.4 local adapter,
existing-item Drive mutation, RM0 capability, live fixture หรือ external action ค่ะ

## 2. Locked safety invariants

ข้อกำหนดต่อไปนี้ห้ามถูกลดระดับใน implementation ค่ะ

1. Existing Drive item content update, rename, move และ Trash เป็น blocked intent
   และห้ามเข้าสู่ executable/retry queue ค่ะ
2. ห้ามสร้าง generic Drive request, permanent-delete, permission mutation หรือ
   OAuth-scope broadening capability ค่ะ
3. Preflight read และ post-verification เป็น evidence แต่ไม่ใช่ CAS ค่ะ
4. Unknown outcome ต้องจบที่ `VerifiedApplied`, `VerifiedNotApplied`,
   `RetrySafe` หรือ `NeedsReconcile` เท่านั้นค่ะ
5. `VerifiedNotApplied` และ `RetrySafe` อนุญาต retry ได้เฉพาะเมื่อ exact current
   preconditions ยังตรงกับ immutable intent และมี approved executor ที่พิสูจน์เงื่อนไข
   นี้ก่อน future claim ค่ะ R3.1 Option A จึง reject ทั้งสอง transition และให้ผู้เรียก
   บันทึก `NeedsReconcile` แทนจนกว่า change-control อื่นจะอนุมัติค่ะ
6. `NeedsReconcile` ห้าม blind retry และห้าม cursor advancement ค่ะ
7. Cursor advance ได้เมื่อทุก declared dependency มี exact committed evidence
   และ transaction เดียวกันลบ active batch หลัง cursor update สำเร็จค่ะ
8. Timestamp, device alias, display name, lexical order และ arrival order เป็น
   explanatory metadata เท่านั้นและห้ามอยู่ใน correctness fingerprint ค่ะ
9. Intent, verification evidence, conflict evidence และ completed tombstone
   ห้ามถูกแก้เพื่อให้ retry ดูเหมือน operation เดิมค่ะ
10. Malformed, newer, negative, partial, constraint-weakened หรือ ambiguous schema
   ต้องถูก preserve และ reject โดยไม่ repair อัตโนมัติค่ะ
11. Credential, bearer token, resumable-session URI, provider/content body,
    ambient Vault path และ raw provider error ห้ามเข้า SQLite ค่ะ

## 3. Schema version decision

R3 durable evidence schema ใช้ `SCHEMA_VERSION = 4` ค่ะ

Fresh database ต้องสร้าง v4 โดยตรงค่ะ Existing exact v3 database ต้อง migrate
ภายใน SQLite transaction เดียวหลัง `quick_check` และ exact v3 schema validation
ผ่านค่ะ Version อื่นหรือ v3 schema ที่ไม่ exact ต้องหยุดก่อนเขียน DDL ใด ๆ ค่ะ

Target v4 รักษา v3 tables และเพิ่ม/ขยาย durable evidence objects ตามหัวข้อ 4–8
ค่ะ SQL statement order และ Rust helper decomposition เป็น implementation detail
แต่ table/field semantics, constraints และ transition rules ในเอกสารนี้ถูก freeze
แล้วค่ะ

Exact v4 schema object set ต้องเพิ่ม tables `mutation_intents`, `mutation_state`,
`mutation_events`, `mutation_verification_evidence` และ `conflict_evidence` ค่ะ
ต้องมี claim index บน phase/due-time/operation identity, operation/attempt indexes
สำหรับ event/evidence lookup และ stable-cell/conflict-copy uniqueness indexes ค่ะ

ต้องมี exact SQLite triggers ที่ reject update/delete ของ `mutation_intents`,
`mutation_events`, `mutation_verification_evidence` และ `conflict_evidence` ค่ะ
การขาดหรือ constraint อ่อนลงของ table, index หรือ trigger ใดต้องทำให้ v4 schema
validation fail closed ค่ะ

## 4. Immutable mutation intent

เพิ่ม table `mutation_intents` เป็น completed-tombstone owner และ immutable
operation identity ค่ะ หนึ่ง `operation_id` มี intent ได้หนึ่งชุดตลอดอายุฐานข้อมูล
ค่ะ

| Field | Contract |
|---|---|
| `operation_id` | Non-nil UUID text, primary key ค่ะ |
| `operation_kind` | `local_publish`, `merge_publish`, `conflict_copy_publish`, `base_publish` หรือ `remote_existing_blocked` ค่ะ |
| `account_id` | Exact verified account identity สำหรับ operation ที่แตะ remote evidence ค่ะ |
| `remote_root_id` | Exact bound root identity สำหรับ operation ที่แตะ remote evidence ค่ะ |
| `remote_file_id` | Exact existing/new remote file identity เมื่อ applicable ค่ะ |
| `source_parent_id` | Exact source parent identity เมื่อ applicable ค่ะ |
| `destination_parent_id` | Exact destination parent identity เมื่อ applicable ค่ะ |
| `local_object_id` | Bounded opaque local object identity เมื่อ platform มี identity ที่พิสูจน์ได้ค่ะ |
| `source_path` | Canonical relative content path ค่ะ |
| `destination_path` | Canonical relative destination path เมื่อ applicable ค่ะ |
| `expected_local_revision` | Exact expected local revision เมื่อ applicable ค่ะ |
| `expected_remote_revision` | Exact expected remote revision/version identifier เมื่อ applicable ค่ะ |
| `base_reference` | Private opaque base-object reference เมื่อ immutable base มีอยู่ค่ะ |
| `base_local_revision` | Exact base local revision เมื่อ applicable ค่ะ |
| `base_remote_revision` | Exact base remote revision/version เมื่อ applicable ค่ะ |
| `base_sha256` | Lowercase SHA-256 ของ immutable base bytes เมื่อ applicable ค่ะ |
| `base_byte_length` | Non-negative immutable base byte length เมื่อ applicable ค่ะ |
| `expected_local_sha256` | Lowercase SHA-256 ของ expected local bytes เมื่อ applicable ค่ะ |
| `expected_local_byte_length` | Non-negative expected local byte length เมื่อ applicable ค่ะ |
| `expected_remote_sha256` | Lowercase SHA-256 ของ expected remote bytes เมื่อ applicable ค่ะ |
| `expected_remote_byte_length` | Non-negative expected remote byte length เมื่อ applicable ค่ะ |
| `operation_marker` | Bounded unique marker ที่ไม่ใช่ credential/capability ค่ะ |
| `intent_fingerprint` | Lowercase SHA-256 ของ canonical correctness fields ค่ะ |
| `registered_at_unix_ms` | Explanatory registration time และไม่อยู่ใน fingerprint ค่ะ |

Hash/size groups ต้องเป็น all-or-none ตาม evidence ที่ applicable ค่ะ
`intent_fingerprint` ต้องรวม operation kind, exact identities, canonical paths,
expected revisions, base reference/hash/size, expected hashes/sizes และ operation
marker ค่ะ ห้ามรวม timestamp, device alias หรือ presentation label ค่ะ

Exact operation-ID rerun ที่ fingerprint ตรงคืน `AlreadyPresent` หรือ
`AlreadyCompleted` ตาม durable state ค่ะ Fingerprint, operation marker หรือ
conflict-copy ID reuse ที่ intent ไม่ตรงต้อง fail closed เป็น collision ค่ะ

`remote_existing_blocked` ต้องมี exact remote file/root/parent/path/revision evidence
เท่าที่ observed ได้ และ registration ต้องสร้าง `NeedsReconcile` disposition
โดยไม่ผ่าน `running` หรือ `retry_scheduled` ค่ะ Enum นี้เป็น durable blocked record
ไม่ใช่ provider capability ค่ะ

## 5. Durable state and append-only events

เพิ่ม table `mutation_state` แบบ one-to-one กับ `mutation_intents` ค่ะ State row
เป็น mutable snapshot เดียวที่เปลี่ยนได้ผ่าน validated transition API เท่านั้นค่ะ

| Field | Contract |
|---|---|
| `operation_id` | Primary/foreign key ไป `mutation_intents` ค่ะ |
| `phase` | `intent_durable`, `running`, `retry_scheduled`, `needs_reconcile` หรือ `completed` ค่ะ |
| `attempt_number` | เริ่มที่ 0 และเพิ่มหนึ่งเมื่อ claim exact retry ค่ะ |
| `state_version` | เริ่มที่ 0 และเพิ่มหนึ่งทุก validated transition เพื่อ reject stale caller โดยไม่ใช้เวลาเป็น correctness input ค่ะ |
| `disposition` | Nullable หรือ `verified_applied`, `verified_not_applied`, `retry_safe`, `needs_reconcile` ค่ะ |
| `next_attempt_at_unix_ms` | มีค่าเฉพาะ `retry_scheduled` ค่ะ |
| `retry_mode` | Nullable หรือ `restart_exact`, `resume_exact` โดยมีค่าเฉพาะ `retry_scheduled` ค่ะ |
| `resume_reference` | Nullable private opaque reference ไป capability store ภายนอก SQLite และมีค่าเฉพาะ `resume_exact` ค่ะ |
| `last_evidence_id` | Exact latest evidence reference เมื่อ disposition ไม่เป็น null ค่ะ |
| `outcome_code` | Nullable bounded redacted code ค่ะ |
| `updated_at_unix_ms` | Explanatory/scheduling time เท่านั้นและห้ามเป็น transition precondition ค่ะ |

เพิ่ม table `mutation_events` เป็น append-only durable history ค่ะ ทุก state
transition ต้อง insert event และ update snapshot ใน transaction เดียวกันค่ะ Event
ประกอบด้วย monotonic `event_id`, operation ID, attempt number, resulting
state version/phase, nullable disposition, nullable evidence ID, redacted outcome
code และ explanatory occurred time ค่ะ Event row ห้าม update/delete ค่ะ

Completed operation ต้องคง intent, state, events และ evidence เป็น tombstone ค่ะ
ห้ามลบ completed row เพื่อเปิดให้ operation ID เดิมทำงานใหม่ค่ะ

## 6. State transition contract

Allowed transitions ถูก freeze ดังนี้ค่ะ

| From | To | Required proof |
|---|---|---|
| registration ค่ะ | `intent_durable` ค่ะ | Valid immutable intent และ initial event commit atomically ค่ะ |
| `intent_durable` ค่ะ | `running` ค่ะ | Claimable kind, exact current preconditions และ attempt increment ค่ะ |
| `running` ค่ะ | `completed/verified_applied` ค่ะ | Exact post-state identity/hash/revision evidence และ no forbidden side effect ค่ะ |
| `running` ค่ะ | `retry_scheduled/verified_not_applied` ค่ะ | Reserved และถูก reject ใน R3.1 Option A ค่ะ Future executor ต้องพิสูจน์ exact pre-state/absence และ refreshed preconditions ก่อน change-control ใหม่ค่ะ |
| `running` ค่ะ | `retry_scheduled/retry_safe` ค่ะ | Reserved และถูก reject ใน R3.1 Option A ค่ะ Future executor ต้องพิสูจน์ provider-confirmed resumable state, exact offset และ revalidated preconditions ก่อน change-control ใหม่ค่ะ |
| `running` ค่ะ | `needs_reconcile/needs_reconcile` ค่ะ | Outcome หรือ preservation proof ไม่เพียงพอค่ะ |
| `needs_reconcile` ค่ะ | `completed/verified_applied` ค่ะ | Explicit reconciliation evidence พิสูจน์ exact applied state ค่ะ |
| `needs_reconcile` ค่ะ | `retry_scheduled/verified_not_applied` ค่ะ | Reserved และถูก reject ใน R3.1 Option A ค่ะ |
| `needs_reconcile` ค่ะ | `retry_scheduled/retry_safe` ค่ะ | Reserved และถูก reject ใน R3.1 Option A ค่ะ |
| `retry_scheduled` ค่ะ | `running` ค่ะ | Due scheduling time, expected state version, same immutable intent และ preconditions revalidated ค่ะ |
| `completed` ค่ะ | `completed` ค่ะ | Exact verify-only rerun ที่ intent และ evidence ตรงค่ะ |

Transition อื่นทั้งหมดถูกปฏิเสธค่ะ โดยเฉพาะ `needs_reconcile -> running`,
`completed -> running`, `running -> intent_durable` และ blocked remote intent
ไป executable phase ค่ะ

ทุก R3 mutation-state API ต้องรับ expected `state_version` หรืออ่าน/compare version ภายใน
transaction เดียวกันค่ะ ห้ามใช้ timestamp, device alias, display name หรือ row
arrival order แทน state version ค่ะ

เมื่อเปิด store หลัง process interruption ทุก `running` row ต้องเปลี่ยนเป็น
`needs_reconcile/needs_reconcile` พร้อม redacted interruption event ใน transaction
เดียวกันค่ะ Store ห้าม claim row ดังกล่าวจน explicit reconciliation บันทึก exact
evidence ตาม allowed transition ค่ะ

### 6.1 Legacy R2 transfer compatibility boundary

Sol change-control A เมื่อ 2026-07-16 อนุญาตให้ v3-to-v4 migration preserve
`transfers.updated_at_unix_ms` และ existing R2 stale-call guards แบบ value exact
เพื่อ compatibility เท่านั้นค่ะ Guard เหล่านี้อนุญาตให้ reject legacy adapter call
ที่เก่ากว่าได้ แต่ timestamp ห้ามเป็น affirmative proof ที่อนุญาต side effect,
เลือก conflict winner, classify conflict, พิสูจน์ applied/not-applied, satisfy R3
dependency หรือ advance cursor ค่ะ

เมื่อ transfer ถูก link เข้ากับ R3 mutation ledger แล้ว `mutation_state.state_version`
และ exact committed evidence เป็น authoritative truth เพียงชุดเดียวค่ะ Legacy
timestamp ห้ามสร้างหรือ advance R3 state และการผ่าน timestamp guard ไม่ทำให้
precondition/evidence อื่นผ่านตามไปด้วยค่ะ

R3.1 migration ไม่ต้อง refactor legacy R2 guards หากยังอยู่หลัง boundary นี้ค่ะ
หาก implementation ทำให้ timestamp เป็น R3 transition/cursor/conflict proof หรือ
ทำให้ legacy guard อนุญาต side effect โดยลำพัง ต้องหยุดและกลับเข้า Sol
change-control ค่ะ

## 7. Verification evidence

เพิ่ม table `mutation_verification_evidence` เป็น append-only exact observation
ค่ะ Evidence ทุก row มี stable `evidence_id`, operation ID, attempt number,
capture phase, disposition, bounded redacted `outcome_code`, observed identity/revisions/hash/size, marker
observation, forbidden-side-effect result, nullable verified received-byte offset,
nullable private opaque resume reference, canonical evidence fingerprint และ
explanatory capture time ค่ะ

`capture_phase` จำกัดที่ `preflight`, `post_verify` และ `reconcile` ค่ะ
`disposition` จำกัดที่ canonical four-class vocabulary ค่ะ Evidence fingerprint
ห้ามรวม capture time, device alias, raw provider body หรือ display name ค่ะ

ข้อกำหนด disposition มีดังนี้ค่ะ

- `VerifiedApplied` ต้องมี exact expected identity/post revision/hash/size เท่าที่
  applicable, marker disposition และ explicit no-forbidden-side-effect result ค่ะ
- `VerifiedNotApplied` และ `RetrySafe` shape ถูก preserve เพื่อ schema/read
  compatibility เท่านั้นค่ะ R3.1 Option A ไม่อนุญาตให้ API record transition สอง
  รูปแบบนี้จนกว่า executor และ exact revalidation proof จะได้รับ change-control ค่ะ
- `NeedsReconcile` ใช้เมื่อ applied/not-applied หรือ preserved evidence พิสูจน์
  ไม่ได้ค่ะ Evidence row ต้องเก็บเฉพาะ bounded reason code ค่ะ

Evidence row ห้าม update/delete ค่ะ Correction ต้องสร้าง evidence ID ใหม่และ
validated transition ต้องอ้าง exact row ที่ใช้ตัดสินค่ะ

## 8. Conflict evidence envelope

เพิ่ม table `conflict_evidence` เป็น append-only classification input/output
record ค่ะ R3.1 เก็บ envelope เท่านั้นและไม่ implement R3.2 classifier ค่ะ

Required fields มี stable `conflict_id`, owning operation ID, stable cell ID,
local state code, remote state code, bounded content class, lineage state,
classification code, ambiguity reason, evidence sufficiency, base/local/remote
hash/size, conflict-copy operation ID, naming version, normalized collision key,
target parent ID และ expected conflict-copy hash/size ค่ะ

Optional explanatory fields จำกัดที่ bounded device alias และ unix time ค่ะ
Fields เหล่านี้ห้ามอยู่ใน conflict identity/fingerprint, uniqueness decision,
winner selection, state classification หรือ cursor query ค่ะ

`conflict_copy_operation_id` ต้อง unique และ reference immutable
`mutation_intents.operation_id` ที่มี kind `conflict_copy_publish` ค่ะ Exact rerun
อ้าง conflict ID/operation ID เดิมและห้ามสร้าง copy identity ใหม่ค่ะ

Conflict evidence row ห้าม update/delete ค่ะ เมื่อ evidence ชุดใหม่เปลี่ยนผล
ต้องสร้าง operation/conflict evidence record ใหม่ตาม R3.2 contract ในอนาคตค่ะ

R3.1 closure candidate มี typed append-only persistence/read API แล้วค่ะ Engine
คำนวณ canonical fingerprint แบบ length-delimited จาก correctness fields เอง และ
reject caller-supplied fingerprint ที่ไม่ตรงค่ะ Device alias และ capture time ไม่อยู่ใน
fingerprint ค่ะ API ตรวจ owning operation, evidence ownership และ
`conflict_copy_publish` kind ก่อน persist โดยไม่ทำ content classification หรือ
materialization ค่ะ

## 9. Cursor dependency contract

Target v4 ขยาย `change_batch_mutations` ให้เป็น typed dependency ledger โดยคงชื่อ
table เพื่อรักษา API/migration surface ค่ะ เพิ่ม fields ต่อไปนี้ค่ะ

| Field | Contract |
|---|---|
| `dependency_kind` | `mutation`, `merge_publication`, `conflict_copy_publication`, `base_publication` หรือ migration-only `legacy_v3` ค่ะ |
| `operation_id` | Foreign key ไป immutable intent สำหรับ R3 dependency ค่ะ |
| `committed_evidence_id` | Foreign key ไป exact verification evidence ที่ทำให้ dependency committed ค่ะ |

State vocabulary ขยายเป็น `pending`, `applying`, `needs_reconcile` และ
`committed` ค่ะ New R3 dependency ต้องมี non-null operation ID ค่ะ การเปลี่ยนเป็น
`committed` ต้องบันทึก evidence reference และ mutation event ใน transaction
เดียวกันค่ะ

Cursor commit ต้อง reject เมื่อมี dependency ที่ไม่ `committed`, ไม่มี exact
operation/evidence reference, disposition ไม่เข้ากับ dependency หรือเป็น
`legacy_v3` ที่ยังไม่ได้ explicit reconciliation ค่ะ Cursor update และ active
batch deletion ต้องอยู่ transaction เดียวกันเหมือน v3 ค่ะ

Step 5 implementation ใช้ typed public request ที่รับ UUID operation identity เท่านั้น
และ map `mutation`/`merge_publication`/`conflict_copy_publication`/`base_publication`
กับ immutable operation kind แบบ one-to-one ค่ะ การ bind dependency ต้อง prove
state `completed` + `verified_applied`, matching `last_evidence_id`, exact
`post_verify` evidence ที่ไม่มี forbidden side effect และ matching completed event ค่ะ
Legacy local-mutation/transfer commit APIs ถูกปฏิเสธเมื่อ batch มี typed R3 dependency
ค่ะ SQLite fault tests ยืนยันว่า abort ก่อน evidence bind หรือ cursor update ไม่ทำให้
dependency/cursor drift ค่ะ

## 10. v3-to-v4 migration preservation

Migration ต้องรักษาข้อมูลต่อไปนี้แบบ byte/value exact เท่าที่ v3 เก็บไว้ค่ะ

- Verified account/root binding และ Vault identity ค่ะ
- Sync phase, tokens, durable cursor และ rescan flag ค่ะ
- Remote entries, exact parent/path/revision/hash และ complete base triples ค่ะ
- Sync queue, completed tombstones และ redacted history ค่ะ
- Transfer intent, marker, phase, retries, private references, verified revisions
  และ transfer history ค่ะ
- Active change batch และทุก v3 mutation row ค่ะ

Migration ห้ามสร้าง R3 mutation intent, verification proof หรือ conflict evidence
จากข้อมูล v3 ที่ไม่พอค่ะ Existing transfer-backed dependency อาจ link exact
transfer operation ID ได้เมื่อ UUID และ transfer row ตรงกันค่ะ แต่ห้ามอ้างว่าเป็น
R3 verification evidence จน exact evidence row ถูกสร้างจาก persisted transfer/base
facts ที่ครบและผ่าน validation ค่ะ

Generic v3 batch row ที่ไม่มี exact R3 operation intent ต้อง migrate เป็น
`legacy_v3` ค่ะ `applying` ต้องกลายเป็น `needs_reconcile` ค่ะ `pending` และ
`committed` ถูก preserve ตาม observed state แต่ cursor gate ต้อง reject
`legacy_v3` ทุก row จน explicit reconciliation สร้าง exact R3 intent/evidence
ค่ะ ห้าม drop active batch หรือ advance cursor ระหว่าง migration ค่ะ

Sol change-control B เมื่อ 2026-07-16 กำหนดให้ existing v3 `sync_jobs` rows ที่มี
kind `move` หรือ `trash` เป็น dormant legacy records ไม่ใช่ provider authorization
หรือ executable R3 intent ค่ะ Migration ต้อง preserve row/value exact แต่ห้าม
backfill row เหล่านี้เข้า `mutation_intents`, ห้ามสร้าง verification evidence จาก
การมี row และห้ามนับ row เป็น R3 change-batch dependency ที่ committed ค่ะ

R3.1 ห้ามเพิ่ม consumer/executor ใหม่ให้ legacy `move`/`trash` queue rows ค่ะ New
R3 existing-item remote mutation ต้อง register เป็น `remote_existing_blocked` และ
atomically จบที่ `NeedsReconcile` โดยไม่ enqueue ค่ะ R3.3 เป็น owner ของ
claim-path filtering, deny tests และ static proof สำหรับ legacy queue surface ค่ะ
ดังนั้น R3.1 artifact ยังไม่ใช่ release candidate สำหรับ remote mutation execution
จน Gate 3 ผ่านค่ะ หาก Step 3 พบ consumer ภายนอก allowlisted sync store/tests หรือ
provider execution path ที่ claim row เหล่านี้ได้ ต้องหยุดเป็น safety finding และ
กลับเข้า change-control ก่อนแก้ source ค่ะ

Migration ต้องสร้าง schema objects, copy/rebuild rows, validate v4 schema,
run foreign-key check และ set `user_version = 4` ก่อน transaction commit ค่ะ
Failure จุดใดต้อง rollback กลับเป็น exact v3 database ค่ะ

## 11. Privacy and validation boundary

Public constructors และ row decoders ต้อง validate UUID, remote identity,
canonical relative path, private opaque reference, revision/hash/size, bounded
enum และ redacted code ก่อน persistence/use ค่ะ Persisted malformed row ต้องทำให้
store fail closed ไม่ normalize หรือ repair ค่ะ

SQLite และ serialized errors ห้ามเก็บหรือเปิดเผยข้อมูลต่อไปนี้ค่ะ

- OAuth credential, access/refresh token หรือ authorization header ค่ะ
- Resumable upload URI, bearer-like session capability หรือ signed URL ค่ะ
- Provider response body, content body หรือ conflict-copy bytes ค่ะ
- Ambient local/Vault path หรือ personal fixture metadata ค่ะ
- Raw provider error/message ที่อาจมี body, ID หรือ credential ค่ะ

Private opaque stage/base/resume reference ที่ผ่าน bounded validator อนุญาตได้
เพราะเป็น identifier ไป private capability store ไม่ใช่ capability เองค่ะ หาก
reference สามารถใช้ authorize provider request ได้โดยตรงต้องถูกปฏิเสธค่ะ

Stable account/root/file IDs และ canonical relative paths เป็น required durable
correctness evidence และอนุญาตเฉพาะ private per-Vault SQLite ตาม existing storage
boundary ค่ะ Logs/status DTOs ต้องใช้ alias/count/redacted code แทน exact IDs ค่ะ

## 12. Step 2 disposition and implementation handoff

Step 2 source/test inventory เสร็จแล้วบน canonical checkpoint ค่ะ Inventory พบ
legacy compatibility findings A/B และคุณโออนุมัติ Sol change-control เมื่อ
2026-07-16 ค่ะ คำตัดสินในหัวข้อ 6.1 และ 10 ไม่เปลี่ยน Option A, ไม่เปิด provider
capability และไม่ลด cursor/unknown-outcome boundary ค่ะ

ผลการ inventory มีดังนี้ค่ะ

1. Current production store ยังเป็น schema v3 และยังไม่มี R3 mutation ledger ค่ะ
2. Legacy R2 transfer timestamp guards ถูกจำกัดตามหัวข้อ 6.1 ค่ะ
3. Legacy v3 `move`/`trash` queue rows ถูกจำกัดตามหัวข้อ 10 ค่ะ
4. agy Gemini 3.5 Flash Medium สองรอบไม่ส่ง accepted inventory output และไม่ถูก
   ใช้เป็น contract evidence ค่ะ
5. Step 3 ใช้ Terra High implement transactional v3-to-v4 migration, exact v4
   schema validation, immutable-record triggers และ bounded tests แล้วค่ะ
6. Step 4 ใช้ Terra High implement immutable intent/state/event/evidence APIs,
   versioned transition, blocked-intent registration และ running-row restart
   recovery แล้วค่ะ

หาก inventory หรือ implementation ต้องเปลี่ยน invariant, enum semantics,
migration disposition, blocked provider boundary หรือ cursor proof ต้องหยุดและ
กลับเข้า Sol High change-control ก่อนแก้ contract/source ค่ะ

### Step 4 change-control disposition

คุณโออนุมัติ Sol change-control `R3.1 Step 4 evidence outcome code` เมื่อ
2026-07-16 ค่ะ จึงเพิ่ม nullable bounded-redacted `outcome_code` ลงใน
`mutation_verification_evidence` และบังคับให้ `NeedsReconcile` evidence มีค่า
ดังกล่าวค่ะ field นี้ต้อง exact เดียวกับ state/event ที่ transition เดียวกันใช้ค่ะ
เพราะ v4 ยังไม่อยู่บน canonical `main` และไม่มี released v4 database การแก้จึงคง
`SCHEMA_VERSION = 4` และแก้ v3-to-v4 target schema โดยไม่สร้าง v5 migration ค่ะ
คำตัดสินนี้ไม่เปลี่ยน Option A, provider boundary หรือ cursor safety ค่ะ

## 13. Step 1 disposition

R3.1 durable evidence contract ถูก freeze บน canonical baseline ที่ระบุด้านบนค่ะ
Closure candidate ใช้ schema v4 แบบ unreleased target, transactional v3-to-v4
migration, immutable intent/state/event/evidence/conflict-envelope records และ typed
cursor dependencies แล้วค่ะ Historical Step 1/4 approval wording ข้างต้นเป็น audit
trail เท่านั้นค่ะ

Gate 1 local closure ครอบคลุม canonical engine fingerprints, exact post-destination
verification, atomic state/event/evidence transitions, conflict-envelope persistence/read
API, exact cursor event equality และ migration/restart/fault regressions ค่ะ R3.2
classifier, merge, local materialization, provider capability และ live action ยังคง
อยู่นอก R3.1 ค่ะ
