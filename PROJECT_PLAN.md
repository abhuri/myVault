# myVault — Locked Product Roadmap

Locked on 2026-07-14 เขตเวลา Asia/Bangkok โดยได้รับ approval จากคุณโอค่ะ

เอกสารนี้เป็นเจ้าของ North Star, release scope, milestone order, exit gates และ change-control rules ค่ะ สถานะ Git กับ operational checkpoint ล่าสุดอยู่ที่ [SESSION_HANDOFF.md](SESSION_HANDOFF.md) ค่ะ หลักฐานการทดสอบอยู่ใน `docs/*/RESULTS.md` และประวัติการเปลี่ยนแปลงอยู่ที่ [CHANGELOG.md](CHANGELOG.md) ค่ะ

## 1. North Star

ส่งมอบแอปส่วนตัวที่เปิด Local Vault หรือ Existing Google Drive Vault ได้อย่างปลอดภัย, ทำงาน offline, Sync โดยไม่เขียนทับข้อมูลเงียบ และกู้คืนหรืออธิบาย conflict ได้บน macOS, Windows, Ubuntu และ Android ค่ะ

งานที่ไม่ช่วยให้เส้นทาง end-to-end นี้ปลอดภัยขึ้นหรือใช้งานได้จริงจะไม่อยู่บน critical path ค่ะ

## 2. นิยาม Project Complete

Project รุ่นปัจจุบันถือว่าเสร็จเมื่อ **Personal First Release** ผ่านทุก release gate ใน R8 ค่ะ

คำว่าเสร็จหมายถึงผู้ใช้คนเดียวสามารถทำ journey ต่อไปนี้ได้จริงค่ะ

1. ติดตั้งและเปิดแอปบน platform เป้าหมายค่ะ
2. เปิดหรือสร้าง Local Vault โดยไม่เปิดเผย native capability สู่ WebView ค่ะ
3. สร้าง, อ่าน, แก้ไข, rename, move, Trash และ Restore เนื้อหาโดยไม่มี silent overwrite ค่ะ
4. เชื่อม Google account ผ่าน native authorization และ bind Existing Drive root ด้วย exact ID ค่ะ
5. ทำงาน offline แล้ว Sync Markdown กับ attachment เมื่อกลับ online ค่ะ
6. Restart ระหว่าง scan, upload, download หรือ mutation ได้โดยไม่ทำข้อมูลหายหรือทำงานซ้ำแบบ blind ค่ะ
7. จัดการ local/remote conflict โดยรักษาทั้งสองฉบับหรือ merge เมื่อพิสูจน์ได้ว่าปลอดภัยค่ะ
8. เห็น Sync status, history, retry, Auth Required, Needs Reconcile และ recovery action ที่เข้าใจได้ค่ะ
9. ค้นหาและนำทาง knowledge core ที่ถูกสร้างจาก full-vault index จริงได้ค่ะ
10. ผ่าน native runtime acceptance บน macOS, Windows, Ubuntu และ physical Android ตาม contract ของแต่ละ platform ค่ะ
11. ผ่าน recovery drill, upgrade/migration test และ release verification บน artifact ที่จะส่งมอบจริงค่ะ

Demo, compile-only, mock-only, emulator-only หรือ foundation-only ไม่ถูกนับว่า Project Complete ค่ะ

## 3. Locked Constraints

- ใช้ Tauri 2, React, TypeScript, Rust, native SQLite และ Google Drive REST API ค่ะ
- แต่ละอุปกรณ์เชื่อม Drive โดยตรงและไม่มี application backend ค่ะ
- รุ่นแรกเป็น personal use และตั้งเป้าค่าใช้จ่ายเงินสด 0 บาทค่ะ
- ไม่มี hosting, domain, VPN หรือ Store distribution ใน Personal First Release ค่ะ
- Markdown กับ attachment ใน Vault เป็น source of truth ส่วน SQLite เป็น derived หรือ operational state ที่สร้างใหม่หรือกู้คืนได้ค่ะ
- Refresh token ต้องอยู่ใน OS secure storage และ token ห้ามอยู่ใน repository, log, SQLite Sync state หรือ WebView ค่ะ
- ไม่มี silent overwrite, permanent delete หรือ automatic conflict-copy deletion ค่ะ
- `.obsidian/` และ `.trash/` เป็น protected paths และไม่อยู่ใน index หรือ Sync ปกติค่ะ
- Android SAF มี durability contract อ่อนกว่า desktop และต้องรายงาน `directorySyncUnsupported` หรือ `writeOutcomeUnknown` ตามจริงค่ะ
- Physical-device หรือ native-OS acceptance ห้ามถูกแทนด้วย compile, mock หรือ emulator เมื่อ contract ต้องการ hardware/runtime จริงค่ะ
- ใช้ disposable fixture หรือ exact allowlisted test root จนกว่า milestone gate จะอนุญาตให้ขยาย scope ค่ะ

## 4. สถานะจริง ณ Roadmap Lock

สถานะรวมโดยประมาณคือ **40–45% ของ Personal First Release เมื่อวัดจาก user-visible outcome** ค่ะ ตัวเลขนี้ใช้กำกับทิศทางและไม่ใช่ earned-value accounting ค่ะ

ฐานเทคนิคไปไกลกว่าตัวผลิตภัณฑ์ที่ผู้ใช้เห็นค่ะ Local safety, recovery และ Sync state foundation มี implementation กับ regression tests จริง แต่ production Drive integration, Sync UI และ cross-device journey ยังไม่ต่อครบค่ะ

| Capability | สถานะจริง | หลักฐานหรือช่องว่างสำคัญ |
|---|---|---|
| Local Vault open, explorer, read และ guarded save | Usable in Demo | macOS live UAT ผ่านค่ะ Android SAF ผ่าน emulator แต่ Windows/Ubuntu native runtime และ physical Android ยัง deferred ค่ะ |
| Local recovery snapshots และ stale-write protection | Runtime integrated | Desktop save publish pre-save snapshot แบบ byte-exact และ fail closed เมื่อ recovery unavailable ค่ะ |
| Create, rename, move, Trash และ Restore | Foundation only | Core/mutation services มี safety tests แต่ Tauri commands และ UI journey ยังไม่ครบค่ะ |
| Editor และ Reader | Partially usable | CodeMirror, autosave, GFM, sanitized Reader, code และ Mermaid ใช้งานได้ค่ะ Attachment workflow, properties และ embeds ยังไม่ครบค่ะ |
| Search, backlinks และ graph | Prototype | Filter/quick switcher และ opened-note backlinks/graph มีใน Demo ค่ะ ยังไม่มี persistent full-vault content index ค่ะ |
| Native Google authorization | Runtime integrated (R1) | Desktop OAuth/Keyring และ Android bridge เชื่อม runtime แล้วค่ะ Android physical-device evidence ยัง deferred ไป R7 ค่ะ |
| Drive REST behavior | Runtime integrated, guarded (R1–R2) | Exact-root binding/read plus create-only resumable upload และ bounded verified download ผ่าน disposable live acceptance แล้วค่ะ Rename/move/Trash/conflict mutation ยังไม่อยู่ใน scope ค่ะ |
| Sync state foundation | Runtime integrated (R1–R2) | Private SQLite, exact binding, lease, scan state, queue, cursor, reconciliation และ durable transfer evidence เชื่อม Tauri runtime แล้วค่ะ |
| Production Drive Sync | Guarded transfer implemented (R1–R2) | Existing Drive binding และ verified transfer อยู่ใน runtime แล้วค่ะ ยังไม่มี conflict handling, full Sync control plane หรือ two-sided mutation UI ค่ะ |
| Packaging และ release | Partial | Demo artifacts และ CI builds มีแล้วค่ะ Native acceptance, recovery guide และ Personal First Release gate ยังไม่ครบค่ะ |

คำว่า `Complete` หมายถึง complete ตามขอบเขต milestone เท่านั้น และไม่แปลว่าผลิตภัณฑ์ทั้งเส้นทางเสร็จแล้วค่ะ

## 5. Personal First Release Scope Freeze

### 5.1 Must Ship

- Local Vault open/create และ persisted activation ตาม platform contract ค่ะ
- Bounded explorer และ local Create/Rename/Move/Trash/Restore ที่ไม่ overwrite ปลายทางเงียบค่ะ
- Revision-checked editor, autosave/manual save, explicit reload และ recovery snapshots ค่ะ
- Sanitized Reader สำหรับ headings, lists, tasks, tables, code, Mermaid และ local images ค่ะ
- Attachment add/open/move/Trash และ Sync โดยตรวจ content hash ค่ะ
- YAML frontmatter preservation โดยไม่ต้องมี visual properties editor เต็มรูปแบบค่ะ
- Wiki-link navigation, basic embeds, persistent full-text search, quick switcher และ backlinks จาก full-vault index ค่ะ
- Basic Local Graph และ Global Graph ที่อ่านจาก persistent index เดียวกันค่ะ
- Native Google authorization, exact Existing Drive binding และ direct Drive Sync ค่ะ
- Offline queue, restart recovery, retry/backoff, unknown-outcome reconciliation และ conflict copies ค่ะ
- Sync status/history/retry/diagnostics ที่ไม่รั่ว token, note body หรือ ambient path ค่ะ
- macOS, Windows, Ubuntu และ physical Android runtime acceptance ค่ะ
- Install, backup, recovery, upgrade และ known-limitations guide ค่ะ

### 5.2 Explicitly Post-release

- Obsidian plugin API และ community plugins ค่ะ
- Canvas แบบเต็มรูปแบบค่ะ
- Dataview-compatible query engine ค่ะ
- Advanced visual properties editor และ database-like views ค่ะ
- Unlinked mentions, semantic search และ AI features ค่ะ
- Arbitrary graph analytics หรือ graph ขนาดใหญ่เกิน release acceptance fixture ค่ะ
- Real-time multi-user collaboration ค่ะ
- End-to-end encryption ค่ะ
- Public publishing, backend, hosting และ VPN ค่ะ
- App Store, Play Store และ Microsoft Store distribution ค่ะ
- Auto-update service และ polished commercial code signing ค่ะ
- iOS target ค่ะ

รายการ Post-release อาจมี prototype ค้างอยู่ใน repository ได้ แต่ห้ามเป็น blocker ของ Personal First Release และห้ามดึงเวลาจาก active milestone ค่ะ

## 6. Locked Execution Rules

- ลำดับบังคับคือ `R1 → R2 → R3 → R4 → R5 → R6 → R7 → R8` ค่ะ
- เปิด implementation milestone ได้ครั้งละหนึ่ง milestone เท่านั้นค่ะ
- ห้ามเริ่ม milestone ถัดไปจน current exit gate ผ่านและคุณโออนุมัติ transition ค่ะ
- ภายใน milestone สามารถแบ่ง bounded parallel tasks ได้ แต่ final integration และ gate ต้องตรวจบน source head เดียวกันค่ะ
- Bug, security finding หรือ data-loss risk ที่อยู่ใน scope ต้องแก้ก่อนผ่าน gate และไม่ถือเป็น scope expansion ค่ะ
- Feature request ใหม่ทั้งหมดเข้า Post-release backlog จนกว่าจะผ่าน change-control ค่ะ
- Planning range เป็นค่าประมาณและไม่ใช่ deadline lock ค่ะ Scope, order และ gates เท่านั้นที่ถูกล็อกค่ะ

Planning range รวมที่เหลือจากผลรวม milestone คือประมาณ **10–19 focused engineering weeks** โดยไม่รวมเวลารอ physical device, native OS environment, external review หรือ account approval ค่ะ

## 7. Locked Roadmap Overview

| Milestone | Outcome | Dependency | Planning range | Status |
|---|---|---|---|---|
| R1 — Native Auth + Read-only Binding | แอปเชื่อม account, bind exact root และอ่าน remote state โดยไม่เขียน Drive ค่ะ | Phase 3A | 1–2 weeks | Complete — merged via PR #26 |
| R2 — Guarded Transfer | Markdown และ attachment upload/download แบบ verified และ restart-safe ค่ะ | R1 | 2–3 weeks | Complete — merged via PR #27 |
| R3 — Safe Conflict Core | Two-sided conflicts, preserve-both และ guarded local materialization ปลอดภัยโดย existing-item Drive mutation ถูก block ค่ะ | R2 | 2–3 weeks | R3.0 content complete — canonicalization/transition pending |
| R4 — Sync Control Plane + Safe Sync Alpha | ผู้ใช้ควบคุมและเข้าใจ Sync ได้ พร้อม end-to-end alpha acceptance ค่ะ | R3 | 1–2 weeks | Locked planned |
| R5 — Local Product Completion | Local CRUD, attachment และ remaining editor/reader journey เชื่อม UI ครบค่ะ | R4 | 1–2 weeks | Locked planned |
| R6 — Knowledge Core | Persistent index, search, links, backlinks และ basic graphs ใช้ full-vault truth ค่ะ | R5 | 1–2 weeks | Locked planned |
| R7 — Cross-platform Runtime Acceptance | Release candidate journey ผ่านบนทุก platform เป้าหมายค่ะ | R6 | 1–3 weeks | Locked planned |
| R8 — Recovery Drill + Personal First Release | Artifact, migration, recovery, docs และ release evidence พร้อมส่งมอบค่ะ | R7 | 1–2 weeks | Locked planned |

## 8. Milestone Specifications

### R1 — Native Auth + Read-only Existing Drive Binding

Outcome คือพิสูจน์ integration ระหว่าง native auth, production Drive adapter, Sync engine และ Tauri โดยยังไม่แก้ remote data ค่ะ

In scope มีดังนี้ค่ะ

- ต่อ Desktop OAuth, token exchange และ OS keyring เข้ากับ Tauri native runtime ค่ะ
- รวม Android authorization provider หลัง native interface เดียวกันเท่าที่ platform contract อนุญาตค่ะ
- สร้าง production Drive read-only adapter โดย reuse validated HTTP behavior จาก spike ค่ะ
- เลือก Existing Drive root และ persist exact account/root ID binding ค่ะ
- ทำ start-token-before-scan, paginated scan, duplicate visibility และ Changes drain ค่ะ
- แสดง read-only preview/status แบบ redacted ค่ะ
- ทดสอบเฉพาะ disposable fixture หรือ exact allowlisted test root ค่ะ

Non-goals คือ upload, download-to-Vault, rename, move, Trash, conflict merge และ continuous background Sync ค่ะ

Exit gate มีดังนี้ค่ะ

- ไม่มี Drive mutation ใน runtime หรือ test log ค่ะ
- Token ไม่ออกสู่ WebView, SQLite, log หรือ serialized error ค่ะ
- Exact account/root binding ปฏิเสธ wrong account, wrong ID และ name-only match ค่ะ
- Initial scan/restart/cursor tests ผ่านโดย local state ไม่ล้ำ remote cursor ค่ะ
- Native macOS read-only fixture journey ผ่านค่ะ Android compile/emulator evidence ต้องผ่าน ส่วน physical Android ยืนยันใน R7 ค่ะ
- Quality, security review, diff review และ documentation evidence ผ่านบน head เดียวกันค่ะ

### R2 — Guarded Upload and Download

Outcome คือ Sync เนื้อหาได้โดยตรวจ bytes และกู้คืนจาก crash/unknown outcome ได้ค่ะ

In scope มีดังนี้ค่ะ

- ต่อ local mutation observation เข้ากับ durable queue ค่ะ
- Verified resumable upload พร้อม exact remote ID/hash reconciliation ค่ะ
- Staged download ไป private temporary area, hash verification และ guarded local publication ค่ะ
- R2 local publication เป็น create-no-replace; existing-different local content
  หยุดที่ `NeedsReconcile` และการ replace ข้าม process/SAF อยู่ใน R3 ค่ะ
- Markdown และ binary attachment transfer ค่ะ
- Retry/backoff, auth expiry, offline pause/resume และ unknown-outcome reconciliation ค่ะ
- Base object/revision capture ที่จำเป็นต่อ R3 conflicts ค่ะ

Exit gate มีดังนี้ค่ะ

- Disposable Local Vault → Drive → second Local Vault round trip ผ่านแบบ byte-exact ค่ะ
- Restart/fault injection ครบทุก persistent transfer boundary ที่มีผลต่อความถูกต้องค่ะ
- Duplicate retry ไม่สร้าง duplicate remote object หรือ overwrite local revision ที่เปลี่ยนแล้วค่ะ
- Hash mismatch, stale revision, quota/network error และ auth expiry fail closed พร้อม recovery action ค่ะ
- ไม่มี cursor advancement ก่อน local commit หรือ verified remote completion ค่ะ

### R3 — Safe Conflict Core

Outcome คือสองอุปกรณ์สังเกตและ classify การแก้ไขพร้อมกันได้โดยไม่มีข้อมูลสูญหาย
หรือ remote deletion ที่กำกวมค่ะ Safe merge, preserve-both และ guarded local
materialization ทำได้ค่ะ Intent ที่ต้อง mutate existing Drive item ต้องหยุดที่
`NeedsReconcile` ค่ะ

In scope มีดังนี้ค่ะ

- Local rename, move, Vault-local Trash, guarded replacement และ conflict-copy
  materialization ผ่าน exact local identity/revision contract ค่ะ
- Remote content/name/parent/removed/trashed observation และ classification โดยใช้
  exact remote identity evidence และไม่มี existing-item Drive mutation ค่ะ
- Markdown three-way merge เมื่อ base/local/remote ชัดและ merge ปลอดภัยค่ะ
- Conflict copy เมื่อ merge ไม่ปลอดภัยหรือเป็น binary attachment ค่ะ
- Delete-versus-edit, rename-versus-edit, move collisions และ duplicate remote paths ค่ะ
- Device/time metadata ที่จำเป็นต่อ conflict explanation โดยไม่พึ่ง timestamp เพื่อความถูกต้องค่ะ
- Existing Drive item content update, rename, move และ remote Trash ถูก block และ
  แยกไป Provider-safe Remote Mutation Gate ที่ไม่เป็น dependency ของ R3 ค่ะ

Exit gate มีดังนี้ค่ะ

- Conflict matrix ครอบคลุม local-only, remote-only, both-changed, delete/edit, rename/edit และ offline replay ค่ะ
- ไม่มี conflict copy ถูกลบอัตโนมัติค่ะ
- Static/runtime evidence ยืนยันว่าไม่มี existing-item `files.update`, remote
  Trash, permanent-delete หรือ generic request capability ค่ะ
- Two-device disposable fixture journey ผ่านพร้อม restart ระหว่าง blocked remote
  intent, guarded local mutation, merge และ conflict publication ค่ะ
- ทุก unknown outcome จบที่ verified completion, retry-safe state หรือ Needs Reconcile ค่ะ

R3.0 Sol High review พบว่า official Drive API v3 surface ที่ตรวจไม่ระบุ
server-enforced expected-revision/conditional mutation สำหรับ existing-item
`files.update` ค่ะ คุณโออนุมัติ Option A change-control เมื่อ 2026-07-16 ให้ลด
R3 scope เป็น Safe Conflict Core และแยก Provider-safe Remote Mutation Gate ออกไป
ค่ะ การลด capability นี้ไม่ลด preserve-both/no-silent-overwrite safety boundary ค่ะ
รายละเอียดอยู่ที่ [R3 safety contracts](docs/sync/R3_CONTRACTS.md) ค่ะ

R3 แบ่ง execution เป็น `R3.0 → R3.1 → {R3.2, R3.3 block enforcement, R3.4} → R3.5 →
R3.6 → R3.7` ค่ะ รายละเอียด outcome, dependency, owner, exit gate, AI staffing
และ usage contract อยู่ที่ [R3 plan](docs/sync/R3_PLAN.md),
[R3 acceptance](docs/sync/R3_ACCEPTANCE.md) และ
[R3 usage ledger](docs/sync/R3_USAGE.md) ค่ะ Safety decisions อยู่ที่
[R3 safety contracts](docs/sync/R3_CONTRACTS.md) ค่ะ Planning pack นี้ไม่ใช่
transition approval และยังห้าม R3 source implementation ค่ะ

### R4 — Sync Control Plane and Safe Sync Alpha

Outcome คือผู้ใช้เห็น, ควบคุม และแก้ปัญหา Sync ได้โดยไม่ต้องอ่าน log หรือ SQLite ค่ะ

In scope มีดังนี้ค่ะ

- Connect/disconnect, bind/unbind, pause/resume และ manual sync ค่ะ
- Status สำหรับ Clean, Local Dirty, Remote Changed, Syncing, Retry Scheduled, Auth Required, Conflict และ Needs Reconcile ค่ะ
- Redacted history, retry action, diagnostics export และ safe recovery guidance ค่ะ
- Background lifecycle policy, app restart และ network transition ค่ะ
- Safe Sync Alpha end-to-end acceptance บน disposable two-device setup ค่ะ

Exit gate มีดังนี้ค่ะ

- User-visible state ตรงกับ durable state หลัง restart ค่ะ
- Retry/action ทุกชนิด idempotent หรือบังคับ explicit reconciliation ค่ะ
- Diagnostics ไม่มี token, note body, absolute Vault path หรือ raw provider response body ค่ะ
- Safe Sync Alpha journey ผ่านบน macOS พร้อม Drive fixture ที่ยืนยัน exact root ค่ะ
- R1–R4 regression matrix, CI และ security review ผ่านบน release-candidate head เดียวกันค่ะ

### R5 — Local Product Completion

Outcome คือ local-only product journey ครบโดยไม่ต้องเรียก core function ผ่าน test harness ค่ะ

In scope มีดังนี้ค่ะ

- Tauri commands และ UI สำหรับ Create/Rename/Move/Trash/Restore ค่ะ
- Local Vault creation และ activation persistence ตาม platform ค่ะ
- Attachment add/open/move/Trash และ local image rendering ค่ะ
- Frontmatter preservation, wiki-link navigation และ basic embeds ค่ะ
- Error/recovery UI สำหรับ stale revision, recovery unavailable และ write outcome unknown ค่ะ
- Accessibility และ compact layout fixes ที่ blocker ต่อ release journey ค่ะ

Exit gate มีดังนี้ค่ะ

- Copy-of-Vault UAT รุ่นใหม่ผ่าน CRUD, attachment, restart และ recovery journey ค่ะ
- Core capability ที่อยู่ใน first-release scope มี Tauri/UI path และ automated integration test ค่ะ
- ไม่มี UI action ที่รับ arbitrary ambient root/path หรือข้าม opaque session boundary ค่ะ
- Dirty buffer, note switch และ external edit ไม่ทำข้อมูลหายค่ะ

### R6 — Persistent Knowledge Core

Outcome คือ search, link และ graph ไม่พึ่งเฉพาะโน้ตที่เคยเปิดค่ะ

In scope มีดังนี้ค่ะ

- Persistent full-vault content index ที่ rebuild จาก Vault ได้ค่ะ
- Incremental index updates จาก watcher และ Sync commits ค่ะ
- Full-text search, quick switcher, wiki-link resolution และ backlinks ค่ะ
- Basic local/global graph จาก persistent index เดียวกันค่ะ
- Tags และ frontmatter fields สำหรับ discovery โดยรักษา unknown fields ค่ะ
- Bounded indexing, malformed-content handling และ schema migration ค่ะ

Exit gate มีดังนี้ค่ะ

- Cold start, rebuild และ incremental update ให้ผล search/link/graph ตรงกันค่ะ
- External editor และ Sync mutation อัปเดต index โดยไม่ index `.obsidian/` หรือ `.trash/` ค่ะ
- Fixture อย่างน้อย 5,000 Markdown notes ผ่าน resource bounds ที่ประกาศค่ะ
- Index เสียหายสามารถ rebuild จาก Vault โดยไม่แก้ source files ค่ะ
- Prototype opened-note graph ถูกแทนหรือถูกลดบทบาทอย่างชัดเจนค่ะ

### R7 — Cross-platform Runtime Acceptance

Outcome คือ release candidate เดียวกันผ่าน native journey บนทุก platform เป้าหมายค่ะ

Platform gates มีดังนี้ค่ะ

| Platform | Required evidence |
|---|---|
| macOS | Install/launch, folder persistence, local CRUD, Keychain restart, Drive Sync, offline/restart, conflict และ recovery drill ค่ะ |
| Windows | Installer/launch, picker persistence, NTFS safety, Credential Manager restart, Drive Sync, conflict และ recovery drill ค่ะ |
| Ubuntu | AppImage/launch, picker persistence, filesystem/ACL behavior, Secret Service restart, Drive Sync, conflict และ recovery drill ค่ะ |
| Physical Android | Sideload/launch, SAF persistence, Google consent/reacquisition, Thai IME, lifecycle/lock-unlock, offline Sync, conflict, recovery outcomes และ real-GPU rendering ค่ะ |

Exit gate มีดังนี้ค่ะ

- Artifact ของแต่ละ platform มาจาก release-candidate source head เดียวกันค่ะ
- Compile, emulator และ native/physical evidence ถูกแยกประเภทชัดเจนค่ะ
- ไม่มี P0/P1 และไม่มี unresolved data-loss, token-leak หรือ silent-overwrite finding ค่ะ
- Platform limitation ทุกข้อมี typed behavior และ user-facing known limitation ค่ะ
- หากไม่มี physical Android device ให้ R7 เป็น Blocked และห้ามเรียก Project Complete ค่ะ

### R8 — Recovery Drill and Personal First Release

Outcome คือ artifact ที่ส่งมอบสามารถติดตั้ง, upgrade, ใช้งาน และกู้คืนได้ตามเอกสารจริงค่ะ

In scope มีดังนี้ค่ะ

- Database/schema upgrade test จาก Demo/previous checkpoint มายัง release candidate ค่ะ
- Backup/export และ restore/import drill สำหรับ disposable Vault ค่ะ
- Simulated interrupted Sync, corrupt derived database, expired auth และ lost network recovery drill ค่ะ
- Final threat-model, dependency, secret, license และ artifact integrity review ค่ะ
- Installation, connection, backup, recovery, troubleshooting และ known-limitations guides ค่ะ
- Version/tag, changelog, checksums และ GitHub Personal Release artifacts ค่ะ

Exit gate มีดังนี้ค่ะ

- Full verification matrix ผ่านบน exact tagged source และ release artifacts ค่ะ
- Fresh install กับ supported upgrade path ผ่านค่ะ
- Recovery runbook ถูกทดลองตามข้อความจริงโดยไม่แตะ personal Vault ค่ะ
- Checksums, artifact names, architecture และ version ตรงกันทุกเอกสารค่ะ
- คุณโอทำ Final Release Review และอนุมัติ publish ค่ะ
- หลัง publish ต้องตรวจ artifact download/install smoke และบันทึก final handoff ค่ะ

## 9. Verification Matrix

| Gate type | R1 | R2 | R3 | R4 | R5 | R6 | R7 | R8 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Unit/property tests | Required | Required | Required | Required | Required | Required | Regression | Regression |
| Integration tests | Required | Required | Required | Required | Required | Required | Required | Required |
| Fault/restart tests | Scan/auth | Transfer | Mutation/conflict | Control plane | Local writes | Index rebuild | Platform journey | Recovery drill |
| Live Drive fixture | Read-only | Read/write | Mutations | Two-device | Regression | Regression | Each platform | Final smoke |
| macOS native UAT | Required | Required | Required | Required | Required | Required | Full | Final |
| Windows native UAT | Compile initially | Compile initially | Compile initially | Build | Targeted | Targeted | Full | Final |
| Ubuntu native UAT | Compile/test | Compile/test | Compile/test | Build | Targeted | Targeted | Full | Final |
| Physical Android | Deferred to R7 | Emulator | Emulator | Emulator | Emulator | Emulator | Full | Final |

ทุก evidence ต้องบันทึก source head, dirty state, environment, command, result และสิ่งที่ deliberately ไม่ได้ทดสอบค่ะ

## 10. Change-control Lock

Roadmap นี้ถูกล็อกตาม approval `Approve lock roadmap` เมื่อ 2026-07-14 ค่ะ

การเปลี่ยน scope, order, exit gate หรือ Personal First Release definition ต้องทำครบทุกข้อดังนี้ค่ะ

1. เขียน Change Request ที่ระบุเหตุผลและปัญหาที่แก้ค่ะ
2. ระบุผลกระทบต่อ data safety, security, architecture, dependencies, test matrix และ planning range ค่ะ
3. ระบุสิ่งที่จะเพิ่ม, ตัด, เลื่อน และหลักฐานว่าทางเลือกเดิมไม่เหมาะสมค่ะ
4. ขอ explicit approval จากคุณโอค่ะ
5. อัปเดตไฟล์นี้, handoff และ evidence owner ที่เกี่ยวข้องใน diff เดียวกันค่ะ

กรณีเดียวที่ interrupt roadmap ได้ทันทีคือ confirmed P0 security/data-loss incident ค่ะ Sunday ต้องหยุด active milestone, preserve evidence, เสนอ incident plan และขอ approval ก่อน remediation ที่เปลี่ยน source ค่ะ

Refactor, dependency upgrade, code splitting หรือ cleanup ทำได้เฉพาะเมื่อจำเป็นต่อ active milestone, ลดความเสี่ยงที่พิสูจน์ได้ หรือได้รับ approval แยกค่ะ

## 11. Architecture and Complexity Guardrails

```text
React UI
  -> Tauri commands ที่รับ opaque session/IDs
  -> Application orchestration
     -> Local Vault services
     -> Native authorization provider
     -> Sync engine
        -> Production Drive adapter
        -> Private operational SQLite

Vault Markdown/attachments = source of truth
Private recovery/sync stores = outside Vault และ disjoint จาก Vault
Phase 0 fixture harness = test/spike only และห้ามกลายเป็น production adapter โดยอ้อม
```

- เพิ่ม crate หรือ state machine ใหม่ได้เมื่อมี trust boundary, platform boundary หรือ independently testable lifecycle ที่ชัดค่ะ
- ห้ามสร้าง abstraction เผื่ออนาคตโดยยังไม่มี consumer ใน active milestone ค่ะ
- R1 ต้อง reuse `desktop-auth`, validated Drive behavior และ `myvault-sync-engine` ก่อนสร้าง layer ใหม่ค่ะ
- Sync operational database ห้ามรับหน้าที่เป็น content index, backlinks database หรือ UI cache ค่ะ
- Prototype ใน `App.tsx` ต้องติดป้าย prototype จนกว่าจะอ่าน full-vault index จริงค่ะ
- เมื่อ capability ยังไม่ต่อถึง UI ให้ใช้คำว่า `Foundation only` แทน `Complete` ค่ะ
- ห้ามเพิ่ม parallel implementation milestone เพื่อเร่งเวลา เพราะจะเพิ่ม integration ambiguity ค่ะ

## 12. Global Definition of Done

Milestone จะถือว่า complete เมื่อครบทุกข้อที่เกี่ยวข้องค่ะ

- มี user-visible หรือ machine-verifiable outcome ที่ตรงกับ milestone ค่ะ
- happy path, error path, restart/retry และ recovery path มีหลักฐานตามระดับความเสี่ยงค่ะ
- ไม่แตะ personal Vault/Drive หาก acceptance ระบุให้ใช้ disposable fixture ค่ะ
- ไม่มี silent overwrite, ambiguous remote binding หรือ cursor advancement ก่อน local commit ค่ะ
- Token, note body, absolute path และ provider response body ไม่รั่วสู่ UI/log โดยไม่จำเป็นค่ะ
- Test, typecheck, build, formatting และ strict lint ที่เกี่ยวข้องผ่านบน source head เดียวกันค่ะ
- Compile/mock/emulator ถูกติดป้ายตามจริงและไม่อ้างเป็น native/physical acceptance ค่ะ
- ไม่มี unresolved P0/P1 และ finding ต่ำกว่าต้องมี owner กับ disposition ค่ะ
- Plan, handoff และ evidence docs ใช้สถานะเดียวกันค่ะ
- Sunday ตรวจ diff, integration impact, scope drift และ final gate แล้วค่ะ
- คุณโออนุมัติ milestone transition ก่อนเริ่ม milestone ถัดไปค่ะ

## 13. Active Risks

| Risk | ระดับ | การควบคุม |
|---|---|---|
| Foundation เยอะกว่าความสามารถที่ผู้ใช้เห็น | High | ใช้ milestone outcome และห้ามนับ library completion เป็น product completion ค่ะ |
| Production integration เชื่อม auth, Drive และ Sync state หลาย trust boundary | High | R1 เป็น read-only gate และห้าม mutation ก่อนผ่านค่ะ |
| Conflict semantics อาจทำข้อมูลหาย | High | R3 ใช้ conflict matrix, preserve both และไม่มี automatic conflict deletion ค่ะ |
| Android SAF ไม่เทียบเท่า desktop atomic filesystem | High | ใช้ typed weaker outcomes และ physical-device acceptance ค่ะ |
| Windows/Ubuntu runtime ยังไม่มี live evidence | High | R7 บังคับ native UAT ก่อน Project Complete ค่ะ |
| Scope กลับไปกว้างแบบ Obsidian clone | High | ใช้ scope freeze และ Post-release list ค่ะ |
| Physical Android availability | Medium | เตรียม runbook/emulator ก่อน แต่ R7 จะ Blocked จนมีเครื่องจริงค่ะ |
| Frontend bundle ใหญ่ประมาณ 1.06 MB | Low | Code splitting ทำเมื่อจำเป็นต่อ R5–R7 performance gate ค่ะ |
| เอกสาร drift | Medium | ใช้ document ownership และ change-control ในไฟล์นี้ค่ะ |

## 14. Document Ownership

- [README.md](README.md) เป็น snapshot สั้นสำหรับคนเข้าใหม่ค่ะ
- ไฟล์นี้เป็นเจ้าของ locked direction, release scope, roadmap, gates และ risks ค่ะ
- [SESSION_HANDOFF.md](SESSION_HANDOFF.md) เป็นเจ้าของ branch, dirty diff, verification ล่าสุด, active milestone และ approval state ค่ะ
- [CHANGELOG.md](CHANGELOG.md) เก็บการเปลี่ยนแปลงตาม release/engineering milestone โดยไม่ทำหน้าที่เป็น handoff ค่ะ
- [docs/sync/R3_PLAN.md](docs/sync/R3_PLAN.md) เป็นเจ้าของ R3.x execution,
  safety contract, parallel ownership และ AI staffing methodology ค่ะ
- [docs/sync/R3_ACCEPTANCE.md](docs/sync/R3_ACCEPTANCE.md) เป็นเจ้าของ R3 gate
  checklist ค่ะ
- [docs/sync/R3_USAGE.md](docs/sync/R3_USAGE.md) เป็นเจ้าของ AI usage vocabulary,
  per-run ledger และ efficiency review ค่ะ
- `docs/*/RESULTS.md` เก็บ evidence พร้อมวันที่และ commit โดยข้อมูลเก่าต้องติดป้าย historical หรือ superseded ค่ะ
- Git และผล command ปัจจุบันเป็น source of truth เมื่อขัดกับ checkpoint ค่ะ

## 15. Current Transition

- R1 ถูก merge เข้า `main` ผ่าน PR #26 ที่ merge commit `681271a` หลัง live
  read-only acceptance, final review, Quality, Android compile, Ubuntu AppImage,
  และ Windows NSIS ผ่านค่ะ
- R2 ถูก merge เข้า `origin/main` ที่ `94db388` ผ่าน PR #27 ค่ะ
- R2 documentation closure baseline คือ `f7a0d7c` บน
  `codex/r2-closure` ค่ะ Baseline นี้ยังต้อง merge เข้า canonical `main` พร้อม
  planning pack นี้หรือ equivalent reviewed diff ค่ะ `f7a0d7c` เป็น post-merge
  narrative เท่านั้นค่ะ R2 source checkpoint ยังคงเป็น merge commit `94db388` ค่ะ
- ไม่มี active implementation milestone ค่ะ R3 — Mutations + Conflict Safety มี
  planning pack `R3.0–R3.7` แล้ว แต่ implementation ยัง locked จน R3.0 gate
  ผ่านและคุณโออนุมัติ transition ใหม่ค่ะ
- macOS disposable byte-exact round trip และ Android API 36 live acceptance
  ผ่านแล้วค่ะ macOS restart upload/download, offline pause/resume, credential
  restoration และ disconnect/reconnect ผ่านแล้วค่ะ Final documentation head
  `b08bb20` ผ่าน exact-head Quality/Android/Ubuntu/Windows CI และ post-merge
  Quality บน `main` ผ่านแล้วค่ะ R2 complete ตาม locked scope ค่ะ
- คุณโออนุมัติ one-time execution สำหรับ R2 แล้วและ execution ปิดสมบูรณ์ค่ะ
  Approval นี้ไม่ครอบคลุม R3 rename/move/Trash/conflict work ค่ะ
- คำขอวันที่ 2026-07-15 อนุญาตให้ทบทวนและบันทึก R3 planning methodology
  ค่ะ คุณโออนุมัติ commit, push และ Draft PR สำหรับ planning/closure documents
  เพิ่มเติมในวันเดียวกันค่ะ Approval นี้ไม่ครอบคลุม R3 source implementation,
  live Drive mutation, PR merge หรือ R3 transition ค่ะ
