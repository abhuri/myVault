# R3.0 — Activation and Safety Contracts

Owner: Sunday ค่ะ

Review route: GPT-5.6 Sol High ค่ะ

Status: `R3.0 CLOSED — OPTION A FROZEN — R3 TRANSITION APPROVED` ค่ะ

เอกสารนี้เป็น canonical R3.0 contract artifact สำหรับ conflict decisions,
merge/preserve-both policy, conflict-copy identity, mutation boundary, provider
semantics, unknown outcomes และ fixture/privacy bounds ค่ะ เอกสารนี้ไม่อนุญาต
source implementation หรือ live Drive mutation ด้วยตัวเองค่ะ R3 transition
ได้รับ approval แยกเมื่อ 2026-07-16 ค่ะ

## 1. Gate decision summary

Google Drive API v3 documentation ที่ตรวจใน R3.0 ไม่ระบุ server-enforced
expected revision, `If-Match`, ETag conditional mutation หรือ compare-and-swap
สำหรับ `files.update` ค่ะ `version` และ `headRevisionId` เป็น output-only และ
preflight read + post-verification ไม่สามารถพิสูจน์ว่าไม่มี concurrent mutation
ถูก overwrite ในช่วงระหว่าง request ได้ค่ะ

คุณโออนุมัติ Option A change-control เมื่อ 2026-07-16 ค่ะ R3 จึงปิด scope เป็น
Safe Conflict Core ที่ classify, merge, preserve both, materialize conflict copy,
เตรียม guarded local mutation เฉพาะเมื่อผ่าน R3.4 platform contract และหยุด
remote-existing-item intent ที่ `NeedsReconcile` ค่ะ Existing Drive item content update, rename, move และ
`trashed=true` คงสถานะ `BLOCKED` และถูกแยกไป Provider-safe Remote Mutation Gate
ซึ่งไม่เป็น dependency ของ R3 Safe Conflict Core ค่ะ

Provider-safe gate เปิดได้เมื่อมี official provider mechanism ที่บังคับ
stale-write rejection บน server และผ่าน independent contract/change-control
review เท่านั้นค่ะ ห้าม expose existing-item mutation ว่า conflict-safe ก่อน gate
ดังกล่าวผ่านค่ะ

ข้อจำกัดนี้ไม่ broaden API surface ค่ะ ห้ามแก้ด้วย generic Drive request,
permanent delete, permission mutation, last-write-wins หรือการใช้ timestamp เป็น
winner ค่ะ Option A resolve scope conflict โดยลด capability ไม่ใช่ลด safety ค่ะ

## 2. Canonical outcome vocabulary

| Outcome | Contract |
|---|---|
| `NoOpVerified` | Exact identity, base และ final state ตรงกันโดยไม่มี side effect ค่ะ |
| `GuardedLocalReplace` | เปลี่ยน local item ผ่าน no-follow, exact lineage/revision recheck, staged bytes, hash verification และ atomic/no-replace capability ที่ platform รองรับค่ะ |
| `SafeTextMergeLocal` | Materialize Markdown merge ที่ผ่าน section 5 locally พร้อมรักษา base/local/remote evidence และยังไม่ advance remote completion cursor ค่ะ |
| `PreserveBothLocal` | รักษาทั้งสอง byte sequences ด้วย deterministic conflict identity และ local no-replace publication ค่ะ |
| `RemoteMutationBlocked` | Durable intent ถูกเก็บได้ แต่ห้ามส่ง existing-item mutation ไป Drive และต้องจบที่ `NeedsReconcile` ค่ะ |
| `NeedsReconcile` | หลักฐานไม่พอ, provider outcome กำกวม หรือ capability อ่อนกว่าสัญญา จึงห้าม side effect/cursor advancement เพิ่มค่ะ |
| `UnsupportedProtected` | Path, topology หรือ provider object อยู่นอก allowlist และต้อง fail closed ค่ะ |

ทุก classifier cell ต้องคืน outcome เดียวพร้อม evidence requirements ค่ะ ห้ามมี
fallback ที่ implementation เลือก policy เองค่ะ Outcome ที่มีคำว่า local เป็น
typed planned result เท่านั้นค่ะ เอกสารนี้ไม่อนุญาต execution และ capability ต้อง
ผ่าน Gate 0 กับ R3.4 platform contract ก่อนค่ะ

## 3. Required decision record

ทุก cell และ durable intent ต้องมี fields ต่อไปนี้เท่าที่ applicable ค่ะ

| Group | Required fields |
|---|---|
| Identity | Account/root aliases, exact remote file ID, source/destination parent IDs, local object identity และ canonical relative path ค่ะ |
| Base evidence | Base revision/version identifiers, base hash/size, local hash/size, remote hash/size และ evidence-capture phase ค่ะ |
| Classification | Stable cell ID, local state, remote state, content class, lineage state, ambiguity reason และ evidence sufficiency ค่ะ |
| Result | Typed outcome, allowed capability, forbidden side effects, bytes/metadata ที่ต้องรักษา และ publication/cursor gate ค่ะ |
| Retry | Operation ID, durable phase, attempt number, previous outcome, post-verification evidence และ terminal unknown-outcome class ค่ะ |
| Conflict copy | Conflict ID, naming version, normalized collision key, target parent, expected hashes และ exact-rerun disposition ค่ะ |
| Fixture | Fixture ID, static/property/fault assertions, expected result และ redacted evidence fields ค่ะ |

Timestamp และ device label เป็น explanatory metadata เท่านั้นค่ะ Filename,
lexical order, mtime, provider time, arrival order และ device label ห้ามเปลี่ยน
classification หรือเลือก winner ค่ะ

## 4. Canonical conflict matrix

คำว่า `remote mutation blocked` ในตารางหมายถึง current Drive provider contract
ตาม section 1 ค่ะ Local materialization ยังต้องผ่าน guarded local capability ค่ะ

| Cell | Local state | Remote state | Required result |
|---|---|---|---|
| `C01` | unchanged | content changed, exact base | `GuardedLocalReplace` หลัง byte/hash verification หรือ `NeedsReconcile` เมื่อ local capability อ่อนกว่า contract ค่ะ |
| `C02` | content changed | unchanged, exact base | รักษา local bytes และ `RemoteMutationBlocked` ค่ะ |
| `C03` | text changed | text changed, non-overlap, exact base | `SafeTextMergeLocal` พร้อม base/local/remote evidence แล้ว `NeedsReconcile` สำหรับ remote publication ค่ะ |
| `C04` | text changed | text changed, overlap | `PreserveBothLocal` ค่ะ |
| `C05` | binary changed | binary changed | `PreserveBothLocal` เสมอค่ะ |
| `C06a` | changed | changed, missing/ambiguous base และ guarded local no-replace publication available | `PreserveBothLocal` ค่ะ |
| `C06b` | changed | changed, missing/ambiguous base และ guarded local publication unavailable/unknown | `NeedsReconcile` โดยรักษา durable evidence ค่ะ |
| `C07` | invalid/ambiguous text encoding | any content change | ห้าม merge และใช้ `PreserveBothLocal` เมื่อ guarded no-replace publication available มิฉะนั้น `NeedsReconcile` ค่ะ |
| `C08` | merge input exceeds bounds | any content change | ห้าม merge และใช้ `PreserveBothLocal` เมื่อ guarded no-replace publication available มิฉะนั้น `NeedsReconcile` ค่ะ |
| `C09` | delete/Trash | edited | รักษา edited remote bytes locally และบันทึก delete intent เป็น `NeedsReconcile` ค่ะ |
| `C10` | edited | delete/Trash | รักษา local bytes, materialize preserve-both evidence และ `NeedsReconcile` ค่ะ |
| `C11` | delete/Trash | delete/Trash, exact identity | `NoOpVerified` เฉพาะเมื่อ final provider state พิสูจน์ได้ มิฉะนั้น `NeedsReconcile` ค่ะ |
| `C12` | rename | edited | ผูก exact identity, materialize content locally โดยไม่ทิ้ง rename intent และ block remote rename ค่ะ |
| `C13` | edited | renamed | ผูก exact identity, apply remote name locally ได้เมื่อ no-collision และรักษา local content แล้ว `NeedsReconcile` ค่ะ |
| `C14a` | rename A | rename A โดย exact intended-name bytes, exact identity และ base ตรงกัน | `NoOpVerified` ค่ะ |
| `C14b` | rename A | rename B ที่ case/Unicode-equivalent แต่ intended-name bytes ต่างกัน | `NeedsReconcile` ค่ะ |
| `C14c` | rename A | rename B ที่ไม่ equivalent | `NeedsReconcile` โดยไม่เลือก winner ค่ะ |
| `C15` | move A | edited | รักษา content และ move intent แล้ว `NeedsReconcile` ค่ะ |
| `C16` | edited | move B | apply remote lineage locally ได้เมื่อ exact/no-collision, รักษา edited bytes และ `NeedsReconcile` ค่ะ |
| `C17` | move A | move A | `NoOpVerified` เมื่อ exact parent/lineage ตรงกันค่ะ |
| `C18` | move A | move B | `NeedsReconcile` โดยไม่เลือกจาก path/time/device ค่ะ |
| `C19` | rename | move | รักษาทั้ง name และ lineage intents; apply locally ได้เฉพาะเมื่อ combined target ไม่ชน แล้ว `NeedsReconcile` ค่ะ |
| `C20` | move/rename | delete/Trash | รักษา local intent/evidence และ `NeedsReconcile` ค่ะ |
| `C21` | delete/Trash | move/rename | รักษา remote identity/metadata evidence และ `NeedsReconcile` ค่ะ |
| `C22a` | destination created | move/rename arrives และ guarded local conflict publication available | ห้าม replace และใช้ `PreserveBothLocal` ค่ะ |
| `C22b` | destination created | move/rename arrives และ guarded local publication unavailable/unknown | `NeedsReconcile` โดยไม่มี replace ค่ะ |
| `C23` | create identity A | create identity B at equivalent path | รักษาทั้งสอง identities และใช้ deterministic conflict-copy plan ค่ะ |
| `C24` | different item moves to target | different item moves to same target | รักษาทั้งสอง identities และ `NeedsReconcile` ค่ะ |
| `C25` | any | duplicate remote path | ห้าม infer identity จากชื่อและใช้ `NeedsReconcile` ค่ะ |
| `C26` | child edited/created | parent moved/trashed | exact-lineage resolution; หากพิสูจน์ไม่ได้ใช้ `NeedsReconcile` ค่ะ |
| `C27` | protected path | any | `UnsupportedProtected` ค่ะ |
| `C28a` | any | shortcut, Google-native, multiple parents หรือ outside-root ancestry | `UnsupportedProtected` โดยไม่มี mutation ค่ะ |
| `C28b` | any | malformed/incomplete metadata ที่ยังระบุ object class ไม่ได้ | `NeedsReconcile` โดยไม่มี mutation ค่ะ |
| `C29` | exact replay | prior outcome `VerifiedApplied` | `NoOpVerified` หลัง exact post-state verification และห้าม side effect ซ้ำค่ะ |
| `C30` | exact replay | prior outcome `VerifiedNotApplied` | retry ได้เฉพาะ capability ที่ allowlisted และ preconditions ยังตรงค่ะ Current existing-item Drive mutation ยังคง blocked ค่ะ |
| `C31` | exact replay | prior side effect unknown | reconcile exact identity/metadata/hash ก่อน และใช้ `NeedsReconcile` เมื่อพิสูจน์ไม่ได้ค่ะ |
| `C32` | offline queued intent | base/revision changedก่อน replay | classify ใหม่ด้วย captured base และห้าม replay side effect เดิมค่ะ |
| `C33a` | any | account/root mismatch หรือ outside-allowlist identity | `UnsupportedProtected` โดยไม่มี mutation ค่ะ |
| `C33b` | any | allowlisted account/root แต่ file/parent/revision/base mismatch | `NeedsReconcile` โดยไม่มี mutation ค่ะ |
| `C34` | case/Unicode-equivalent destination | any arrival | ถือเป็น collision, ห้าม replace และใช้ conflict-copy identity ค่ะ |

Matrix นี้ freeze safety result ที่ fail closed ค่ะ Cell IDs ที่มี suffix เป็น
canonical sub-cells และแต่ละ predicate คืน outcome เดียวค่ะ Remote capability ที่ถูก block
ไม่ถือว่า implemented หรือ accepted และทุก cell ต้องมี fixture ก่อน R3.2 ค่ะ

## 5. Text, binary and Markdown merge contract

- Merge eligibility จำกัดที่ regular Markdown file ซึ่ง base/local/remote เป็น
  valid UTF-8 และแต่ละ version ไม่เกิน 4 MiB, 100,000 logical lines และ combined
  decoded input ไม่เกิน 12 MiB ค่ะ เกินขอบเขตต้อง preserve both ค่ะ
- Binary, unknown MIME/extension, invalid UTF-8, ambiguous BOM หรือ decode error
  ต้อง preserve both ค่ะ ห้าม best-effort transcoding ค่ะ
- Three-way merge ทำได้เมื่อ base exact, edits ไม่ overlap และ identity/lineage
  ไม่กำกวมค่ะ Conflict marker ใน user content ไม่ใช่ accepted merge result ค่ะ
- YAML frontmatter คือ region จาก opening `---` ที่ byte zero ถึง closing `---`
  ค่ะ ห้าม semantic YAML merge ค่ะ หากทั้งสองฝั่งแก้ frontmatter ต้อง preserve both
  ค่ะ หากฝั่งเดียวแก้ให้คง bytes ของฝั่งนั้นค่ะ
- Newline style และ final newline เป็น merge attributes ค่ะ หากทั้งสองฝั่งใช้
  style เดียวกันให้คง style นั้นค่ะ หากหนึ่งฝั่งตรง base ให้ใช้ style ของฝั่งที่
  เปลี่ยนค่ะ หากทั้งสองเปลี่ยนต่างกันให้ preserve both ค่ะ
- ก่อน merge ต้องรักษา exact base/local/remote hashes ค่ะ Output ต้อง hash และ
  verify หลัง guarded local publication ค่ะ
- Merge หรือ conflict materialization ห้าม advance remote completion cursor จน
  durable evidence และ publication outcome ถูก commit ค่ะ

## 6. Conflict-copy identity and naming

- `conflict_id` ต้อง derive แบบ domain-separated จาก operation version, exact
  account/root aliases, exact object identity, base evidence, local/remote hashes
  และ canonical classification ค่ะ ห้ามใช้ timestamp, device label หรือ arrival
  order เป็น identity input ค่ะ
- Display name ใช้ original stem เพื่อให้ผู้ใช้เข้าใจได้ และต่อท้าย
  ` (conflict <id-prefix>)` ก่อน extension ค่ะ Correctness ผูกกับ `conflict_id`
  ไม่ใช่ display name ค่ะ
- R3.2 Sol mechanics freeze กำหนด portable collision comparison key v1 เป็น NFKC,
  Unicode default full case folding และ NFKC ต่อ component พร้อม canonical
  length-delimited hierarchy serialization และ platform-invalid-name checks ค่ะ
  Persisted `normalized_collision_key` evidence เก็บ lowercase SHA-256 digest ของ
  canonical sequence เพื่อคง ASCII-only/redacted contract ค่ะ Naming version
  `r3-conflict-name-v1-nfkc17-casefold9-nfkc` pin normalization data Unicode 17.0.0
  และ case-fold data Unicode 9.0.0 ค่ะ Exact rerun ต้องใช้ persisted version เดิมค่ะ
- หาก prefix ชนคนละ `conflict_id` ให้ขยาย deterministic ID prefix ค่ะ ห้ามใช้
  timestamp หรือ first-free numeric suffix เป็น correctness mechanism ค่ะ
- Exact rerun ที่พบ same `conflict_id` และ expected hashes ต้อง verify/reuse ค่ะ
  ห้ามสร้าง copy ซ้ำค่ะ Same name แต่ different identity/hash ต้อง no-replace และ
  เลือก deterministic longer-ID name ค่ะ
- Conflict copy ห้ามถูกลบอัตโนมัติค่ะ `.obsidian/` และ `.trash/` ไม่ใช่ target
  ของ normal conflict-copy publication ค่ะ

## 7. Mutation capability boundary

### 7.1 Current production allowlist

| Capability | Current R3.0 decision |
|---|---|
| Existing-content update via `files.update` | `BLOCKED` และอยู่นอก R3 Safe Conflict Core เพราะไม่มี documented server-enforced stale-write precondition ค่ะ |
| Rename existing Drive item | `BLOCKED` ด้วยเหตุผลเดียวกันค่ะ |
| Move existing Drive item via `addParents`/`removeParents` | `BLOCKED` ด้วยเหตุผลเดียวกันและต้องพิสูจน์ exact single-parent topology ค่ะ |
| Trash existing Drive item via `trashed=true` | `BLOCKED` ด้วยเหตุผลเดียวกันและห้ามแทนด้วย permanent delete ค่ะ |
| Generic Drive request | `PROHIBITED` ค่ะ |
| HTTP `DELETE` / permanent delete | `PROHIBITED` ค่ะ |
| Permission mutation / OAuth-scope broadening | `PROHIBITED` จนมี approval แยกค่ะ |
| Read-only exact-ID reconciliation | ใช้ได้เฉพาะ capability ที่ R1/R2 อนุมัติแล้วและต้องไม่ log provider/content body ค่ะ |
| Guarded local mutation | ยังต้องผ่าน R3.4 platform contract และไม่อนุญาตจากเอกสารนี้ค่ะ |

Preflight version/revision check และ post-verification ยังเป็น required evidence
แต่ไม่ใช่ replacement สำหรับ atomic precondition ค่ะ การเห็น final state ตรงกับ
intent พิสูจน์ได้เพียง observed final state ไม่ได้พิสูจน์ว่าไม่มี concurrent value
ถูก overwrite ค่ะ

### 7.2 Supported topology bounds

- Exact approved account/root, one exact file ID และ zero/one exact parent ตาม
  provider object contract เท่านั้นค่ะ
- Destination ต้องอยู่ใต้ exact allowlisted disposable root และ ancestry ต้อง
  re-fetch/verify ก่อน operation ค่ะ
- Shortcuts, Google-native content, multiple parents, shared-drive topology,
  cross-root move, permission mutation, redirect/origin change และ malformed
  metadata ต้อง fail closed ค่ะ
- Vault-local Trash เป็น local capability แยกจาก remote `trashed=true` ค่ะ

### 7.3 R3.4 blocked capability disposition and R3.5 handoff

R3.4 capability proof จบที่ `open / blocked by prerequisites` ค่ะ ไม่อนุญาตให้
Desktop หรือ Android SAF execute guarded replace, rename, move, Vault-local Trash หรือ
conflict-copy publication จาก contract นี้จนกว่าจะพิสูจน์ทุก precondition ก่อน side
effect ได้แก่ held root, exact source identity, source revision, destination parent,
collision set และ no-replace semantics ค่ะ

Desktop foundation ที่มี revision check, held root/parent, collision scan และ
recovery journal ไม่ใช่ proof ของ durable exact source identity หรือ atomic replacement
ค่ะ Android SAF ที่คืน unavailable → `NeedsReconcile` เป็น fail-closed result ที่ถูกต้อง
แต่ไม่ใช่ evidence ว่ามี guarded capability ค่ะ

R3.5 ต้องรับ controlled change proposal สำหรับ durable identity,
journal/recovery/replay semantics, final-outcome classification และ watcher/SAF echo
idempotency ค่ะ Proposal ต้องผ่าน Sol High change-control disposition และได้รับ
explicit user approval ที่จำกัด exact source/test scope ก่อน source write ค่ะ
Documentation closeout approval อย่างเดียวไม่เปิด implementation ค่ะ ห้าม reinterpret
`FileRevision`, preflight/post-verify หรือ watcher hint เป็น replacement proof ค่ะ
ก่อน completion gate ของ R3.4 ทุก unknown/unsupported result ต้องเป็น
`WriteOutcomeUnknown` หรือ `NeedsReconcile` ตาม evidence และห้าม advance cursor ค่ะ

## 8. Provider semantics record

Official references ที่ review มีดังนี้ค่ะ

- [Files.update](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/update)
  ใช้ exact `{fileId}` และรองรับ metadata/content patch, `addParents` และ
  `removeParents` ค่ะ Parameter surface ที่เผยแพร่ไม่ระบุ conditional mutation ค่ะ
- [File resource](https://developers.google.com/workspace/drive/api/reference/rest/v3/files)
  ระบุ `version` และ binary `headRevisionId` เป็น output-only ค่ะ
- [Manage folders](https://developers.google.com/workspace/drive/api/guides/folder)
  ระบุ move ผ่าน `addParents`/`removeParents` และ single-parent topology ค่ะ
- [V2 to V3 reference](https://developers.google.com/workspace/drive/api/guides/v2-to-v3-reference)
  map Trash เป็น `files.update` พร้อม `trashed=true` ค่ะ
- `trashed` อาจสืบทอดจาก trashed parent และผู้ใช้ที่ไม่ใช่ owner ไม่มีสิทธิ์ Trash
  item ค่ะ หาก remote Trash ถูกเสนอให้ unblock ต้องแยก explicit/inherited state
  และ owner capability ก่อนค่ะ
- [Manage uploads](https://developers.google.com/workspace/drive/api/guides/manage-uploads)
  ให้ resumable-session status query สำหรับ interrupted upload ค่ะ กลไกนี้ไม่ใช่
  CAS สำหรับ metadata/rename/move/Trash ค่ะ
- [Manage revisions](https://developers.google.com/workspace/drive/api/guides/manage-revisions)
  ระบุข้อจำกัด retention/completeness จึงห้ามใช้ revision history อย่างเดียวเป็น
  correctness proof ค่ะ

การสรุปว่าไม่มี supported CAS เป็น inference จาก official resource/update
surface ที่ตรวจค่ะ หากพบ official mechanism ใหม่ต้อง reopen R3.0 contract และ
review ด้วย Sol High ก่อน code ค่ะ

## 9. Unknown-outcome taxonomy

| Terminal class | Required proof | Retry rule |
|---|---|---|
| `VerifiedApplied` | Exact identity และ post-state metadata/hash ตรง durable intent พร้อม evidence ว่า no forbidden side effect เกิดค่ะ | Exact rerun เป็น verify-only ค่ะ |
| `VerifiedNotApplied` | Exact pre-state ยังอยู่และไม่มี operation marker/expected state ค่ะ | Retry ได้เฉพาะ allowlisted capability และ preconditions ใหม่ยังตรงค่ะ |
| `RetrySafe` | Exact resumable session status query คืน `308` พร้อม received range ค่ะ `200/201` หมายถึง complete และต้อง verify ส่วน `404` หมายถึง session expired ค่ะ | Resume ได้เฉพาะ exact held session URI จาก offset ที่ provider ยืนยันค่ะ ห้าม resume หลัง complete/expired ค่ะ |
| `NeedsReconcile` | Applied/not-applied หรือ preserved evidence พิสูจน์ไม่ได้ค่ะ | ห้าม blind retry และห้าม cursor advancement ค่ะ |

สำหรับ existing-item Drive mutation ใน current contract ห้ามส่ง request ตั้งแต่
ต้นค่ะ หาก evidence แสดงว่า request หลุดออกไปต้อง classify เป็น incident และ
`NeedsReconcile` ไม่ใช่ retry path ค่ะ

## 10. Fixture and privacy boundary

- Gate fixtures ใช้ approved disposable account, exact disposable Drive root และ
  disposable local Vault A/B เท่านั้นค่ะ Repo เก็บได้เฉพาะ stable aliases และ
  redacted fingerprints ค่ะ Actual IDs, paths, credentials และ tokens อยู่ใน
  private runtime allowlist ภายนอก repository ค่ะ
- Gate 0 freeze เฉพาะ fixture schema, aliases, redaction และ data boundary ค่ะ
  Exact disposable account/root fingerprints ต้องได้รับ approval ก่อน R3.6 live
  R1/R2 regression เท่านั้นค่ะ การมี schema ไม่ใช่ live fixture approval ค่ะ
- Required deterministic fixtures ครอบคลุมทุก canonical `C01–C34` รวม sub-cells, Thai/Unicode NFC/NFD,
  case-only collision, restart, lost response, protected path และ exact rerun ค่ะ
- AI workers รับเฉพาะ allowlisted docs/fixtures ที่ไม่มี content body, OAuth,
  keyring, provider body, ambient Vault path หรือ personal Drive metadata ค่ะ
- WebView, SQLite, serialized errors, logs และ usage ledger ห้ามเก็บ token,
  provider/content body, resumable capability หรือ ambient path ค่ะ
- Raw AI output อยู่ภายนอก repository, retention สั้นและ permission จำกัดค่ะ
  Ledger บันทึกเฉพาะ model ที่ observable, scope, wall/exit, accepted work และ
  privacy-safe usage fields ค่ะ

## 11. Gate 0 disposition

Sol High review และ Option A approval ปิด conflict outcomes, merge fallback,
conflict-copy identity, provider semantics, fail-closed capability boundary,
fixture schema/privacy bounds และ Safe Conflict Core scope แล้วค่ะ R3.0 content
freeze และ canonicalization complete ผ่าน PR #28 ที่ `main@eb6709c` ค่ะ Quality
run `29461969032` ผ่านบน exact source head `f120679` ก่อน merge ค่ะ

คุณโอให้ explicit `Approve R3 transition` เมื่อ 2026-07-16 และ Gate 0 ถูก
ประเมินผ่านบน canonical
`main@9a30ad9763b8a9503484f2a35e559b1c7ee800b6` ค่ะ R3.1 Step 1 durable
evidence/schema contract ถูก freeze แยกใน
[R3_1_DURABLE_EVIDENCE_CONTRACT.md](R3_1_DURABLE_EVIDENCE_CONTRACT.md) ค่ะ
Option A และ provider boundary ในเอกสารนี้ไม่เปลี่ยนค่ะ
