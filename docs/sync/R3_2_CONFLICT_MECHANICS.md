# R3.2 — Pure Conflict Mechanics Freeze

Owner: Sunday ค่ะ

Review route: GPT-5.6 Sol High ค่ะ

Status: `R3.2 IMPLEMENTED — TERRA AND SOL CLOSURE AUDITS CLEAN LOCALLY` ค่ะ

เอกสารนี้ freeze deterministic mechanics สำหรับ pure classifier, Markdown merge,
conflict identity/naming และ local materialization plan ค่ะ เอกสารนี้ไม่อนุญาต
filesystem, Tauri, provider หรือ Drive side effect ค่ะ Existing-item Drive content
update, rename, move และ Trash ยังคง blocked ตาม Option A ค่ะ

## 1. Pure input and output boundary

Classifier รับ immutable value objects เท่านั้นค่ะ Input correctness fields มี exact
account/root/object identity, stable cell ID, canonical source/destination path,
parent lineage, base/local/remote revision, lowercase SHA-256/byte length, bounded
content class, durable state version และ prior verified outcome ค่ะ

Public classification route ต้องสร้าง `ClassificationEvidence` และผ่าน
`ConflictInput::new`/`fresh` ก่อนค่ะ Approved input ที่ขาด exact identity, canonical
content path, revisions, durable state version หรือ case-required fingerprints ถูก reject
ก่อน matrix evaluationค่ะ `NonOverlappingMarkdownChanges` ต้องแนบ exact verified merge
fingerprint ที่มาจาก bounded Markdown engine; orchestration consumer ต้องห้ามสร้าง proof
จาก timestamp/name/order ค่ะ

Timestamp, device alias, display label, lexical order และ arrival order ห้ามอยู่ใน
predicate, identity, winner selection หรือ retry decision ค่ะ

ผลลัพธ์เป็น typed plan หนึ่งค่าใน vocabulary ต่อไปนี้ค่ะ

- `NoOpVerified` ค่ะ
- `GuardedLocalReplace` ค่ะ
- `SafeTextMergeLocal` ค่ะ
- `PreserveBothLocal` ค่ะ
- `RemoteMutationBlocked` ค่ะ
- `UnsupportedProtected` ค่ะ
- `NeedsReconcile` ค่ะ

Typed plan ประกอบด้วย immutable operation drafts และ dependency DAG เท่านั้นค่ะ
ห้าม claim, execute, verify side effect, advance cursor หรือเรียก persistence ภายใน
classifier/materializer ค่ะ R3.1 APIs เป็น consumer boundary แยกต่างหากค่ะ

## 2. Decision precedence

Classifier ต้องประเมินตามลำดับนี้ค่ะ

1. ตรวจ account/root/protected/object-class boundary ค่ะ
2. ตรวจ prior verified outcome, exact rerun และ unknown outcome ค่ะ
3. ตรวจ identity และ parent/path lineage โดยห้าม infer จากชื่อค่ะ
4. ตรวจ canonical destination และ portable collision set ค่ะ
5. จำแนก local/remote content and metadata state เทียบ immutable base ค่ะ
6. ประเมิน Markdown merge eligibility หรือ preserve-both requirement ค่ะ
7. สร้าง typed local publication/base publication/blocked-remote drafts ค่ะ

Guard ที่ fail ต้องคืน typed safe result ทันทีและห้าม fall through ไปผลที่มี
capability สูงกว่าค่ะ ทุก canonical `C01–C34` และ sub-cell ต้อง map ไป predicate
ชุดเดียวและผลเดียวค่ะ

## 3. Markdown bounds and parsing

Merge candidate ต้องเป็น regular Markdown และ base/local/remote ต้องครบค่ะ แต่ละ
version ต้อง valid UTF-8, ไม่ขึ้นต้นด้วย BOM, ไม่เกิน 4 MiB และไม่เกิน 100,000
logical linesค่ะ Combined decoded input ต้องไม่เกิน 12 MiB ค่ะ
ก่อนเรียก Myers ต้องผ่าน deterministic work guard
`base_line_count × max(local_line_count, remote_line_count) <= 100,000,000` ค่ะ
เกิน guard ให้ preserve both/`NeedsReconcile` ตาม local capability โดยไม่เริ่ม diff ค่ะ

Bare CR, mixed LF/CRLF ภายใน version เดียว, ambiguous/unclosed frontmatter หรือ
decode error ให้ `PreserveBothLocal` เมื่อ guarded no-replace plan มีได้ มิฉะนั้นให้
`NeedsReconcile` ค่ะ Binary/unknown content ห้ามเข้า text merge ค่ะ

Logical line content แยกจาก newline attribute ก่อน diff ค่ะ Newline style และ final
newline เลือกตาม frozen R3 contract แล้ว render output หลัง content merge เพื่อไม่ให้
newline-only change กลายเป็น content overlap ค่ะ

Frontmatter มีได้เมื่อ byte zero เริ่มด้วย logical line `---` และมี closing logical line
`---` ค่ะ Region รวม opening/closing lines ค่ะ หากทั้ง local และ remote เปลี่ยน byte
content ใดใน region เมื่อเทียบ base ต้อง preserve both แม้ line edits ไม่ทับกันค่ะ

## 4. Deterministic edit and overlap mechanics

Base-to-local และ base-to-remote edit scripts ใช้ deterministic Myers-style line
diff จาก logical UTF-8 line bytes ค่ะ Implementation ต้อง pin dependency/version และ
มี repeated-line golden fixtures เพื่อจับ tie-break drift ค่ะ

หนึ่ง edit คือ half-open base range กับ replacement line bytes ค่ะ Mechanics มีดังนี้ค่ะ

- Exact same range และ exact same replacement bytes ให้ coalesce หนึ่งครั้งค่ะ
- Non-empty ranges ที่มี interior intersection ถือว่า overlap ค่ะ
- Different insertions ที่ base anchor เดียวกันถือว่า overlap ค่ะ
- Insertion ที่ anchor แตะ start/end ของ non-empty edit อีกฝั่งถือว่า overlap ค่ะ
- Adjacent non-empty ranges `[a,b)` และ `[b,c)` ไม่ overlap ค่ะ
- Empty identical insertions ที่ anchor เดียวกัน coalesce ได้ค่ะ
- Edit script ambiguity, invalid index หรือ reconstruction mismatch ต้อง preserve bothค่ะ

Merged output ต้อง reconstruct จาก base + ordered non-overlap edits และ hash/length
ต้องคำนวณจาก output bytesค่ะ ห้าม emit conflict marker ลง user content ค่ะ

## 5. Newline and frontmatter selection

หาก local/remote newline style ตรงกันให้ใช้ style นั้นค่ะ หากฝั่งหนึ่งตรง base ให้ใช้
style ของฝั่งที่เปลี่ยนค่ะ หากทั้งสองเปลี่ยนเป็นคนละ style ให้ preserve both ค่ะ Rule
เดียวกันใช้กับ final-newline boolean ค่ะ

Frontmatter ที่เปลี่ยนฝั่งเดียวใช้ logical content bytes ของฝั่งนั้นค่ะ Body edits อีก
ฝั่ง merge ได้เมื่อไม่ overlap ค่ะ หากทั้ง local และ remote เปลี่ยน frontmatter จาก
base ต้อง preserve both เสมอ แม้ผลแก้ทั้งสองฝั่งจะมี exact bytes ตรงกันค่ะ

## 6. Conflict identity

`conflict_id` เป็น lowercase SHA-256 hex จาก domain
`myvault-r3-conflict-id-v1` และ canonical length-delimited field stream ค่ะ Field
order ถูก freeze ดังนี้ค่ะ

1. identity version ค่ะ
2. exact account ID ค่ะ
3. exact remote-root ID ค่ะ
4. exact object identity ค่ะ
5. stable cell ID ค่ะ
6. canonical classification code ค่ะ
7. canonical identity path ค่ะ
8. target parent identity ค่ะ
9. base hash/length หรือ explicit absent marker ค่ะ
10. local hash/length ค่ะ
11. remote hash/length ค่ะ
12. naming version ค่ะ

ทุก field encode เป็น field-name length, field-name bytes, presence byte, value length
และ value bytesค่ะ ห้ามใช้ delimiter ที่ caller-controlled bytes ทำให้กำกวมค่ะ

Canonical identity path เลือกจาก exact base lineage ก่อนค่ะ หากไม่มี base ให้ใช้
canonical path ของ local-existing identity และหากไม่มี local identity ให้ใช้ canonical
path ของ remote-only identityค่ะ หากมีหลาย candidate หรือ exact identity/path binding
พิสูจน์ไม่ได้ต้อง `NeedsReconcile` ค่ะ Target parent identity คือ exact local parent ที่
conflict copy จะถูกวางหลัง lineage resolution ไม่ใช่ remote display path ค่ะ

Conflict-copy operation ID derive ด้วย UUID v5 จาก fixed namespace
`a8357e62-4e5d-5a88-9169-913b906d46cf` และ bytes
`conflict-copy\0<conflict_id>` ค่ะ Merge/base operation drafts ใช้ namespace เดียวกัน
แต่แยก domain `merge-publish` และ `base-publish` ค่ะ Exact rerun ต้องได้ UUID เดิมค่ะ

## 7. Portable collision and naming version

Naming version v1 คือ
`r3-conflict-name-v1-nfkc17-casefold9-nfkc` ค่ะ Collision key ใช้ canonical `/`
separators และ NFKC → Unicode default full case fold → NFKC ต่อ component ค่ะ
Normalization data คือ Unicode 17.0.0 และ case-fold data คือ Unicode 9.0.0 ตาม
pinned dependencies ปัจจุบันค่ะ

การอัปเกรด Unicode data หรือ algorithm ต้องสร้าง naming version ใหม่ค่ะ Exact rerun
ต้องใช้ persisted naming version เดิมและห้าม recompute identity ด้วย latest versionค่ะ

Normalized component ที่มี `/`, NUL หรือ platform-invalid scalar หลัง normalization
ต้องถูก reject ก่อน join ค่ะ Collision-key serialization ใช้ length-delimited normalized
components แล้วคั่น hierarchy ด้วย canonical separator token ที่ caller สร้างไม่ได้ค่ะ
ห้าม join raw normalized components ด้วย `/` เพียงอย่างเดียวค่ะ
การเปรียบเทียบในหน่วยความจำใช้ canonical component sequence นี้โดยตรงค่ะ ส่วน
`normalized_collision_key` ที่ persist เป็น evidence ต้องเก็บ lowercase SHA-256 digest
ของ sequence เพื่อคง R3.1 ASCII-only และ redacted-field contract ค่ะ

Display name ใช้ original stem แล้วต่อ ` (conflict <prefix>)` ก่อน extension ค่ะ Prefix
เริ่ม 12 lowercase hex characters และขยายครั้งละ 4 characters เมื่อ portable collision
key ชน different conflict ID ค่ะ Same conflict ID + expected hash/length ให้ verify/reuse
ค่ะ เมื่อใช้ full 64 characters แล้วยังพบ mismatched identity/hash ให้ `NeedsReconcile`
โดยห้าม timestamp หรือ numeric first-free suffix ค่ะ

Stem truncation เพื่อ portable component bound ต้องทำที่ Unicode scalar boundary และ
เป็น pure function ของ original stem, extension, suffix และ naming versionค่ะ Candidate
ทุกชื่อจำเป็นต้องผ่าน `VaultPath` portable/protected-path validation ค่ะ

## 8. Materialization drafts and cursor dependencies

`GuardedLocalReplace` สร้าง `local_publish` draft ค่ะ `SafeTextMergeLocal` สร้าง
`merge_publish` และ `base_publish` draftsค่ะ `PreserveBothLocal` สร้าง
`conflict_copy_publish` และ `base_publish` draftsเมื่อ exact new base มีได้ค่ะ
สำหรับ `PreserveBothLocal` canonical local bytes อยู่ที่ path เดิม และ conflict copy
ใช้ exact remote bytes ค่ะ การเลือกตาม stable local/remote role นี้ไม่ขึ้นกับ arrival orderค่ะ
Remote-existing intent สร้างเฉพาะ `remote_existing_blocked` draft ที่จบ
`NeedsReconcile` ค่ะ

Plan ต้องระบุ expected hashes/lengths, exact source/destination paths, parent/object
identities, operation marker, operation IDs และ typed cursor dependency kindค่ะ Cursor
ห้าม advance จน consumer persist exact completed evidence/event สำหรับ dependency ทุก
ตัวตาม R3.1 contract ค่ะ

Conflict copy ไม่มี delete draft และ `.obsidian/`/`.trash/` ห้ามเป็น destination ค่ะ
Execution DAG บังคับให้ `base_publish` มี local/merge/conflict-copy publication ของ plan
เดียวกันเป็น prerequisites ค่ะ Flat R3.1 cursor dependency set ยังคงใช้เป็น durable
completion gate แต่ไม่แทน execution ordering edge ค่ะ

`C07`/`C08` ใช้ `PreserveBothLocal` เฉพาะเมื่อ guarded no-replace conflict publication
availableค่ะ มิฉะนั้นต้อง `NeedsReconcile` ค่ะ `C12` คืน `RemoteMutationBlocked`
พร้อม guarded local-content publication draft และ retained local rename intentค่ะ `C13`
คืน `NeedsReconcile` พร้อม optional guarded local-rename draft เฉพาะเมื่อ exact identity,
lineage และ destination no-collision พิสูจน์ได้ค่ะ Offline `C11` ไม่มี final-state proof
จึงต้อง `NeedsReconcile` ค่ะ
Rename/move cycle map ไป `C18 NeedsReconcile` โดยไม่มี local metadata draft ค่ะ

## 9. Stop and change-control conditions

หยุดและกลับ Sol change-control เมื่อ implementation ต้องเปลี่ยน matrix result,
normalization/naming version, conflict-ID field set, overlap rule, Option A,
R3.1 schema semantics หรือ cursor proof ค่ะ หยุดทันทีหากต้องใช้ provider,
filesystem/Tauri side effect, personal data, credential หรือ live Drive ค่ะ
