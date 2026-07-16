# R3 — AI Worker Usage and Efficiency Ledger

Owner: Sunday ค่ะ

Status: `MEASUREMENT CONTRACT ACTIVE — R3.1 CLOSURE RECORDED` ค่ะ

Phase routing baseline reviewed 2026-07-16 Asia/Bangkok ค่ะ Runtime availability
ยังต้องตรวจซ้ำตอนเปิดทุก R3 session ค่ะ

เอกสารนี้ป้องกันการเรียก context tokens, quota percentage, credits และ billable
tokens ปนกันค่ะ เป้าหมายคือใช้ GPT subagents และ Antigravity workers เท่าที่เพิ่ม
accepted engineering work จริง โดยไม่ส่ง credential, personal data หรือ broad
repository context โดยไม่จำเป็นค่ะ

## 1. Measurement vocabulary

| Term | Meaning |
|---|---|
| `reported_tokens` | Token fields ที่ execution surface ส่งออกอย่างเป็นทางการค่ะ |
| `context_tokens` | Token ที่อยู่ใน context window ค่ะ ไม่เท่ากับ billing หรือ quota debit เสมอค่ะ |
| `quota_usage` | Provider quota/bucket ที่เหลือหรือถูกใช้ค่ะ ห้ามแปลงเป็น token หาก provider ไม่ให้สูตรค่ะ |
| `credit_usage` | Overage หรือ purchased credits ค่ะ แยกจาก baseline quota ค่ะ |
| `accepted_work_unit` | ผลงานที่ reviewer ยืนยันว่าใช้ได้จริง เช่น matrix cell, fixture, finding หรือ call site ค่ะ |
| `first_pass_acceptance_rate` | Accepted outputs หารด้วย reviewed outputs ใน batch เดียวค่ะ |

## 2. OpenAI measurement contract

Official Codex guidance ระบุว่า subagents ช่วยแยก noisy context แต่แต่ละ child ทำ
model/tool work ของตัวเอง จึงใช้ token รวมมากกว่า comparable single-agent run ค่ะ

### Interactive Codex

- `/status` ใช้ดู task/session configuration, context usage และ token usage ค่ะ
- `/usage` ใช้ดู daily, weekly หรือ cumulative ChatGPT token activity และ rate-limit
  reset ที่ account surface รองรับค่ะ
- `/statusline` สามารถแสดง model, context, limits, tokens และ session fields ค่ะ
- Account/workspace analytics เป็น aggregated reporting ค่ะ ห้ามใช้เป็น exact
  per-workflow cost attribution ค่ะ

### Non-interactive Codex

`codex exec --json` คืน JSONL event `turn.completed.usage` ซึ่งมีอย่างน้อย
`input_tokens`, `cached_input_tokens`, `output_tokens` และ
`reasoning_output_tokens` ค่ะ นี่เป็นวิธีที่ต้องใช้เมื่อ R3 worker ต้อง pin model
และต้องการ per-run reported tokens ค่ะ

ตัวอย่าง bounded read-only worker ค่ะ

```bash
codex exec \
  --ephemeral \
  --json \
  --model gpt-5.6-terra \
  --config 'model_reasoning_effort="medium"' \
  --sandbox read-only \
  --cd /tmp/myvault-r3-scoped \
  "Return a concise source-backed report only."
```

Consumer ต้องเก็บเฉพาะ final agent message และ `turn.completed.usage` ค่ะ ห้าม
commit raw JSONL ที่อาจมี command, path, prompt หรือ source content ค่ะ

### Native app subagents

Current `spawn_agent` surface ไม่ส่ง model selector หรือ per-child usage fields ให้
main agent ค่ะ ทุก run ต้องบันทึก `model=runtime-selected/unobservable` และ
`reported_tokens=unavailable` ค่ะ ห้ามประมาณ token จากความยาว summary แล้วเรียก
เป็น provider usage ค่ะ

## 3. Antigravity CLI measurement contract

Local executable ที่ validate แล้วคือ `agy 1.1.2` ค่ะ

### Official interactive metrics

- `/usage` และ `/quota` เป็น alias ที่ refresh Model Quotas จาก backend ค่ะ
- `/context` และ statusline JSON แสดง context-window input/output/cache fields ค่ะ
- `/credits` แสดง overage credit balance/usage เมื่อเปิดใช้ credits ค่ะ
- Quota, context และ credits เป็นคนละหน่วยและห้ามแทนกันค่ะ

Antigravity plans ระบุว่า baseline limits สัมพันธ์กับ amount of work และแต่ละ
prompt ใช้ quota ไม่เท่ากันค่ะ ดังนั้น quota delta 1% ห้ามถูกตีความเป็น token
จำนวนคงที่ค่ะ

### Headless limitation

`agy --help` รุ่น 1.1.2 ไม่มี supported machine-readable usage export ค่ะ
`--print` ไม่คืน exact token usage ต่อ run ค่ะ Internal SQLite BLOB, backend
protocol หรือ private log ห้ามถูก scrape/decoded เป็น usage contract ค่ะ

Official statusline JSON มี `context_window.total_input_tokens`,
`total_output_tokens`, `context_window_size`, `used_percentage`,
`remaining_percentage` และ `current_usage` cache fields ค่ะ แต่ automated capture
ใน headless `--print` ยังเป็น `unverified` จนกว่าจะมี synthetic validation และ
approval แยกค่ะ

Statusline payload ยังมี email, cwd, workspace/project URI, plan tier และ
conversation ID ค่ะ Collector ต้อง parse ใน memory, allowlist เฉพาะ CLI version,
model display name, timestamp ที่ wrapper สร้างเองและ `context_window` counters,
แล้ว discard fields อื่นก่อน persist ค่ะ ห้ามเขียน raw statusline payload ลง disk
แม้เป็น private log ค่ะ Run linkage ใช้ locally generated `run_id` แทน provider
conversation ID ค่ะ Default คือ `context_metrics=not captured` จน synthetic test
พิสูจน์ privacy filter และ headless coverage ค่ะ

Antigravity SDK มี per-turn/cumulative `usage_metadata` สำหรับ prompt, cached,
candidate และ thinking tokens ค่ะ SDK เป็นคนละ execution surface กับ `agy` และ
ไม่อยู่ใน R3 workflow จนกว่าจะมี decision/security review แยกค่ะ

### Safe agy run contract

```bash
agy \
  --model "Gemini 3.5 Flash (Medium)" \
  --mode plan \
  --sandbox \
  --new-project \
  --print-timeout 3m \
  --print "Return a direct bounded report without delegation."
```

- รันจาก fresh temporary workspace ที่มีเฉพาะ allowlisted input files ค่ะ
- ใช้ fresh conversation ต่อ bounded task เพื่อไม่ให้ context จาก run เก่าปนค่ะ
- Prompt ระบุ direct response, no delegation, no file write และ output cap ค่ะ
- Raw log ถ้าจำเป็นต้องอยู่ใน private directory mode `0700`, file mode `0600`
  และ retention สั้นค่ะ
- ห้ามอ่านหรือส่ง OAuth token, settings secret, personal Vault หรือ personal Drive ค่ะ

## 4. Run ledger schema

ทุก AI worker run บันทึก fields ต่อไปนี้เท่าที่ surface รองรับค่ะ

| Field | Rule |
|---|---|
| `run_id` | Stable local identifier ที่ไม่เปิดเผย conversation credential ค่ะ |
| `phase` | `R3.0` ถึง `R3.7` ค่ะ |
| `provider_surface` | `codex-app-spawn`, `codex-exec` หรือ `agy-cli` ค่ะ |
| `role` | integrator, explorer, implementer, reviewer, fixture หรือ log analyst ค่ะ |
| `model` | Exact model เมื่อ observable หรือ `runtime-selected/unobservable` ค่ะ |
| `effort_or_tier` | OpenAI effort หรือ Flash Low/Medium/High ค่ะ |
| `mode` | read-only, plan, workspace-write หรือ integration-owner ค่ะ |
| `source_sha` | Git SHA ที่ worker อ่านหรือแก้ค่ะ |
| `input_scope` | Allowlisted files/directories โดยไม่ใส่ personal path ค่ะ |
| `started_at` / `finished_at` | Asia/Bangkok timestamp ค่ะ |
| `wall_seconds` / `exit_code` | Wrapper-observed execution data ค่ะ |
| `reported_tokens` | Provider-supported fields หรือ `unavailable` ค่ะ |
| `quota_before_after` | Redacted Agy batch snapshot ID หรือ `not captured` ค่ะ |
| `context_metrics` | Measured, unavailable หรือ unverified ค่ะ |
| `accepted_work_units` | Reviewer-confirmed count และชนิดงานค่ะ |
| `review_result` | accepted, partial, rejected หรือ failed ค่ะ |
| `notes` | Concise redacted limitation/failure ค่ะ |

## 5. R3 run ledger

| Run ID | Phase | Surface | Role/model | Scope | Usage | Result |
|---|---|---|---|---|---|---|
| `r3.0-gap-native-01` | R3.0 | `codex-app-spawn` | reviewer / `runtime-selected/unobservable` ค่ะ | R3 plan minimum matrix, Gate 0 และ usage clauses ค่ะ | Per-child tokens unavailable ค่ะ | Accepted gap inventory 1 ชุดค่ะ |
| `r3.0-drive-docs-01` | R3.0 | `codex-app-spawn` | explorer / `runtime-selected/unobservable` ค่ะ | Official Drive files/update, folders, uploads และ revisions docs ค่ะ | Per-child tokens unavailable ค่ะ | Accepted provider-semantics report 1 ชุดค่ะ |
| `r3.0-gap-agy-01` | R3.0 | `agy-cli` | reviewer / Gemini 3.5 Flash (Medium) ค่ะ | Allowlisted R3 plan/acceptance/usage copies ใน temporary sandbox ค่ะ | Wall 21.1s; exact tokens/quota unavailable ค่ะ | Output accepted ค่ะ Wrapper exit `1` เกิดหลัง output จาก zsh reserved variable และ temporary workspace ถูกลบแล้วค่ะ |
| `r3.0-option-a-consistency-02` | R3.0 | `codex-app-spawn` | reviewer / `runtime-selected/unobservable` ค่ะ | Option A contracts/plan/acceptance/project/readme/handoff consistency ค่ะ | Per-child tokens unavailable ค่ะ | Accepted ค่ะ P1 approval wording และ legacy matrix finding ถูกแก้และ re-review ผ่านค่ะ |
| `r3.0-option-a-provider-02` | R3.0 | `codex-app-spawn` | reviewer / `runtime-selected/unobservable` ค่ะ | Option A provider boundary, R1/R2 create-only bound และ RM0 reopening rule ค่ะ | Per-child tokens unavailable ค่ะ | Accepted ค่ะ ไม่พบ P1 provider contradiction หลังแก้ค่ะ |
| `r3.1-schema-inventory-agy-01` | R3.1 | `agy-cli` | reviewer / Gemini 3.5 Flash (Medium) ค่ะ | Allowlisted contract/store/test/adapter copies ใน isolated temporary sandbox ค่ะ | Wall approximately 12s; exact tokens/quota unavailable ค่ะ | Failed ค่ะ Tool confirmation ถูก soft-deny ก่อน inventory และไม่มี output ที่ยอมรับค่ะ |
| `r3.1-schema-inventory-agy-02` | R3.1 | `agy-cli` | reviewer / Gemini 3.5 Flash (Medium) ค่ะ | Correction run ใน isolated temporary sandbox เดิมโดยไม่เปิด repository write ค่ะ | Wall approximately 34s; exact tokens/quota unavailable ค่ะ | Failed ค่ะ CLI พยายามอ่าน unavailable worktree/parent metadata และไม่มี output ที่ยอมรับค่ะ Temporary workspace/log ถูกลบแล้วค่ะ |
| `r3.1-step3-schema-test-inventory-01` | R3.1 | `codex-app-spawn` | reviewer / `runtime-selected/unobservable` ค่ะ | Read-only migration/store/test inventory ใน allowlisted sync-engine files ค่ะ | Per-child tokens unavailable ค่ะ | Accepted hotspot and minimal-test inventory ค่ะ ไม่มี file write หรือ external action ค่ะ |
| `r3.1-step3-terra-01` | R3.1 | `interactive-codex` | integrator / GPT-5.6 Terra High ค่ะ | Transactional v3-to-v4 schema migration, exact validation, immutable triggers และ bounded Rust tests ค่ะ | Exact tokens/quota unavailable ค่ะ | Accepted implementation work ค่ะ `myvault-sync-engine` suite ผ่าน 48 tests ค่ะ |
| `r3.1-step4-state-inventory-01` | R3.1 | `codex-app-spawn` | reviewer / `runtime-selected/unobservable` ค่ะ | Read-only state/evidence/restart API and test inventory ใน allowlisted sync-engine files ค่ะ | Per-child tokens unavailable ค่ะ | Accepted inventory ค่ะ ไม่มี file write หรือ external action ค่ะ |
| `r3.1-step4-terra-01` | R3.1 | `interactive-codex` | integrator / GPT-5.6 Terra High ค่ะ | Outcome-code change control, immutable ledger API, versioned transition, restart recovery และ bounded Rust tests ค่ะ | Exact tokens/quota unavailable ค่ะ | Accepted implementation work ค่ะ `myvault-sync-engine` suite ผ่าน 51 tests ค่ะ |
| `r3.1-step5-cursor-inventory-01` | R3.1 | `codex-app-spawn` | reviewer / `runtime-selected/unobservable` ค่ะ | Read-only typed dependency/cursor/fault-test inventory ใน allowlisted sync-engine files ค่ะ | Per-child tokens unavailable ค่ะ | Accepted inventory ค่ะ ไม่มี file write หรือ external action ค่ะ |
| `r3.1-step5-terra-01` | R3.1 | `interactive-codex` | integrator / GPT-5.6 Terra High ค่ะ | Typed R3 dependency registration, exact evidence/event cursor gate, legacy API exclusion, bounded SQLite fault tests และ lint cleanup ค่ะ | Exact tokens/quota unavailable ค่ะ | Accepted implementation work ค่ะ full `myvault-sync-engine` suite ผ่าน 55 tests และ strict Clippy ผ่านค่ะ |
| `r3.1-step6-scope-audit-01` | R3.1 | `codex-app-spawn` | reviewer / `runtime-selected/unobservable` ค่ะ | Read-only adversarial scope/durable-field/cursor-bypass inventory ใน allowlisted diff and sync-engine files ค่ะ | Per-child tokens unavailable ค่ะ | Accepted audit report ค่ะ ไม่มี file write, test หรือ external action ค่ะ |
| `r3.1-step6-terra-01` | R3.1 | `interactive-codex` | validator / GPT-5.6 Terra High ค่ะ | Focused migration/state/cursor/fault tests, full engine/transfer suites, strict lint and Gate 1 evidence mapping ค่ะ | Exact tokens/quota unavailable ค่ะ | Passed local candidate validation ค่ะ engine 55 tests, transfer 15 tests, format, strict Clippy และ diff check ผ่านค่ะ |
| `r3.1-sol-audit-01` | R3.1 | `interactive-codex` | auditor / GPT-5.6 Sol High ค่ะ | Frozen-contract audit of durable evidence binding, cursor proof and unknown-outcome retry semantics ค่ะ | Exact tokens/quota unavailable ค่ะ | One evidence-binding finding fixed and revalidatedค่ะ One unknown-outcome semantic blocker requires explicit change-controlค่ะ |
| `r3.1-option-a-terra-01` | R3.1 | `interactive-codex` | integrator / GPT-5.6 Terra High ค่ะ | Implement Sol-approved Option A by rejecting R3.1 retry transitions and adding atomic regression coverage ค่ะ | Exact tokens/quota unavailable ค่ะ | Full final validation passedค่ะ Engine 57 tests, transfer 15 tests, format, strict Clippy และ diff check ผ่านค่ะ |
| `r3.1-closure-audit-01` | R3.1 | `codex-app-spawn` | reviewer / `runtime-selected/unobservable` ค่ะ | Read-only adversarial closure audit of schema, fingerprints, conflict envelope, cursor semantics and scope drift ค่ะ | Per-child tokens unavailable ค่ะ | Accepted report ค่ะ พบ 3 P1 และ 2 P2 โดยไม่มี P0 ค่ะ |
| `r3.1-closure-main-01` | R3.1 | `interactive-codex` | integrator / current session ค่ะ | Engine-owned canonical fingerprints, post-destination verification, immutable conflict-envelope API, cursor semantic equality, regression suite and document closure ค่ะ | Exact tokens/quota unavailable ค่ะ | Published implementation `main@c774324` ค่ะ Engine 61 tests, transfer 15 tests, format, strict Clippy และ diff check ผ่านค่ะ |
| `r3.2-matrix-agy-01` | R3.2 | `agy-cli` 1.1.3 | reviewer / Gemini 3.5 Flash (Medium) ค่ะ | Allowlisted R3 contracts, Gate 2 acceptance และ mechanics freeze copiesใน isolated temporary sandbox ค่ะ | Wall approximately 34s; exact tokens/quota/context unavailable ค่ะ | Partial acceptedค่ะ รับ 4 predicate/test findings, downgrade Option A cursor finding เป็น expected fail-closed boundary และลบ temporary workspaceแล้วค่ะ |

R3.0 rows ข้างต้นเป็น planning/contract evidence เท่านั้นค่ะ R3.1 agy rows เป็น
failed bounded inventory attempts และไม่มี accepted work ค่ะ ทุก row ไม่ใช่ R3
source implementation earned work และไม่มี provider-supported exact token record ค่ะ
Smoke test และ planning pilots ที่ทำก่อน R3 activation ยังคงเป็น methodology
evidence เท่านั้นค่ะ

### Pre-R3 methodology evidence

| Run | Surface/model | Observed usage | Review result |
|---|---|---|---|
| Zero-repository smoke | agy / Gemini 3.5 Flash (Low) ค่ะ | Wall time 8.4s ค่ะ Exact tokens/quota unavailable ค่ะ | Accepted ค่ะ |
| Matrix pilot v1 | agy / Gemini 3.5 Flash (Medium) ค่ะ | Exact tokens/quota unavailable ค่ะ | Failed output capture ค่ะ |
| Matrix pilot v2 | agy / Gemini 3.5 Flash (Medium) ค่ะ | Wall time 21.5s ค่ะ Exact tokens/quota unavailable ค่ะ | Partial, useful first pass ค่ะ |
| Planning-doc review | agy / Gemini 3.5 Flash (Medium) ค่ะ | Wall time approximately 30s ค่ะ Exact tokens/quota unavailable ค่ะ | Partial, three wording/policy findings reviewed by Sunday ค่ะ |

ตารางนี้จงใจไม่สร้าง token estimate จาก output length ค่ะ Quota snapshots ไม่ถูก
เก็บเพราะ `/usage` เป็น interactive TUI และอาจแสดง account metadata ค่ะ

## 6. Batch efficiency review

ประเมินทุก 3–5 bounded workers หรือเมื่อเปลี่ยน model/tier ค่ะ

- `accepted_work_units / quota_delta` ใช้กับ agy เมื่อมี comparable snapshots และ
  snapshot method ผ่าน synthetic privacy validation แล้วเท่านั้นค่ะ
- `accepted_work_units / reported_tokens` ใช้กับ `codex exec --json` ค่ะ
- `first_pass_acceptance_rate` ใช้ทุก surface ค่ะ
- `review_minutes / accepted_work_unit` ใช้วัดภาระที่ผลลัพธ์โยนกลับให้ Sunday ค่ะ
- `failed_runs / total_runs` ใช้ตรวจ orchestration/prompt reliability ค่ะ

Worker/model ต้องถูกปรับหรือลดการใช้เมื่อเกิดข้อใดข้อหนึ่งค่ะ

- First-pass acceptance ต่ำกว่า 60% สอง batch ติดต่อกันค่ะ
- Reviewer ใช้เวลาตรวจมากกว่าทำงานเองโดยไม่มี coverage gain ค่ะ
- Output ไม่มี source references หรือมี confirmed hallucination ซ้ำค่ะ
- Run ต้อง retry เพราะ delegation/output contract ซ้ำมากกว่าหนึ่งครั้งค่ะ
- Worker ขอข้อมูลหรือสิทธิ์เกิน allowlist ค่ะ

## 7. Phase model routing and session bootstrap

ตารางนี้เป็น canonical source สำหรับ model routing ของ R3 ค่ะ ก่อนเริ่มแต่ละ
R3.x phase ให้ตรวจ model availability ใน current runtime และ official model
guidance อีกครั้งค่ะ หากชื่อรุ่นหรือ effort ในตารางไม่พร้อมใช้ Sunday ต้องเสนอ
current equivalent พร้อมเหตุผลก่อนเริ่มงาน ห้าม silently substitute ค่ะ

### 7.1 R3.x model matrix

| Phase | Main Sunday default | Sunday gate/escalation | Antigravity CLI | Intended work |
|---|---|---|---|---|
| `R3.0` | GPT-5.6 Terra Medium ค่ะ | GPT-5.6 Sol High ตอน freeze conflict matrix, mutation boundary และ Gate 0 ค่ะ | Gemini 3.5 Flash (Medium) สำหรับ matrix expansion และ gap scan ค่ะ | Contract/evidence drafting ใช้ Terra ส่วน safety decision ใช้ Sol ค่ะ |
| `R3.1` | GPT-5.6 Sol High สำหรับ schema, durable state และ crash boundaries ค่ะ | ลดเป็น GPT-5.6 Terra High ได้หลัง contract ล็อกและเหลือ bounded implementation/test ค่ะ | Gemini 3.5 Flash (Medium) สำหรับ schema/test inventory ค่ะ | Migration, cursor gating และ restart correctness ค่ะ |
| `R3.2` | GPT-5.6 Terra High สำหรับ pure classifier, merge implementation และ fixtures ค่ะ | GPT-5.6 Sol High สำหรับ ambiguous conflict policy และ Gate 2 ค่ะ | Gemini 3.5 Flash (Medium) เป็นค่าเริ่มต้น และ High สำหรับ bounded adversarial combinations ค่ะ | Deterministic classification, preserve-both และ property fixtures ค่ะ |
| `R3.3` | GPT-5.6 Sol High สำหรับ mutation-block boundary และ negative capability proof ค่ะ | GPT-5.6 Terra High ใช้ได้เฉพาะ bounded block-enforcement tests หลัง Option A contract freeze ค่ะ | Gemini 3.5 Flash (High) สำหรับ second opinion จาก allowlisted official excerpts ค่ะ | `RemoteMutationBlocked`, static no-update/no-DELETE audit, cursor withholding และ provider-gate evidence ค่ะ |
| `R3.4` | GPT-5.6 Terra High ค่ะ | GPT-5.6 Sol High สำหรับ Android SAF unknown outcomes, replacement safety และ Gate 4 ค่ะ | Gemini 3.5 Flash (Medium) สำหรับ Desktop/SAF capability matrix ค่ะ | Guarded local adapters และ weaker provider outcomes ค่ะ |
| `R3.5` | GPT-5.6 Sol High ค่ะ | GPT-5.6 Sol Extra High เฉพาะ final adversarial review หรือ P0/P1 ambiguity ค่ะ | Gemini 3.5 Flash (Medium) สำหรับ trace grouping และ transition coverage ค่ะ | Integrated mutation/conflict state machine และ cursor correctness ค่ะ |
| `R3.6` | GPT-5.6 Terra Medium ค่ะ | GPT-5.6 Sol High เฉพาะ unexplained failure, inconsistent state หรือ potential data loss ค่ะ | Gemini 3.5 Flash (Low) สำหรับ log grouping และ Medium สำหรับ evidence matrix ค่ะ | Test execution, fault injection, triage และ evidence collection ค่ะ |
| `R3.7` | GPT-5.6 Sol High ค่ะ | Extra High ใช้เฉพาะ unresolved P0/P1 ที่ต้อง deep review ค่ะ | Gemini 3.5 Flash (Medium) สำหรับ independent documentation consistency review ค่ะ | Final diff, security, scope drift และ merge-readiness decision ค่ะ |

Luna Low/Medium ใช้กับ structured worker สำหรับ extraction, classification,
inventory และ documentation consistency ค่ะ Luna ไม่เป็น Main Sunday default ใน
R3 เพราะทุก phase ยังมี cross-component judgment และ approval boundary ค่ะ

### 7.2 Model switching and escalation rules

- ใช้ model เดิมตลอด bounded implementation segment และเปลี่ยน model ที่ phase
  boundary หรือ planned gate แทนการสลับทุก turn ค่ะ
- Terra ต้องยกระดับเป็น Sol ทันทีเมื่อ ambiguity กระทบ data preservation,
  exact identity, deletion/Trash, provider precondition, cursor commit หรือ
  unknown outcome ค่ะ
- Sol Extra High ใช้เฉพาะจุดที่ตารางระบุหรือเมื่อ confirmed P0/P1 finding ต้อง
  adversarial reasoning เพิ่มค่ะ ห้ามใช้เป็น default ทั้ง phase ค่ะ
- ถ้า first-pass acceptance ต่ำกว่า 60% สอง batch ติดต่อกัน ให้ปรับ prompt,
  ลด scope หรือยกระดับ model ตามลำดับค่ะ
- หาก Sol ไม่เพิ่ม accepted work หรือ coverage อย่างวัดได้เมื่อเทียบกับ Terra
  บนงานชนิดเดิม ให้กลับไปใช้ Terra สำหรับ bounded segment ถัดไปค่ะ
- `agy` ช่วยวิเคราะห์และ review เท่านั้นค่ะ ห้ามใช้ผลจาก `agy` เป็น final safety
  decision, approval, live Drive action, credential action, commit หรือ merge ค่ะ
- Native `spawn_agent` ที่ไม่เปิด model/usage fields ต้องบันทึก
  `model=runtime-selected/unobservable` ค่ะ หากต้อง pin model ให้ใช้ bounded
  `codex exec --ephemeral --json --model ...` ใน scoped workspace ค่ะ

### 7.3 Required declaration at the start of every R3 session

ใน response แรกของทุก session ที่เกี่ยวกับ R3 Sunday ต้องอ่าน
`SESSION_HANDOFF.md`, phase section ใน `R3_PLAN.md`, applicable gate ใน
`R3_ACCEPTANCE.md` และตารางนี้ก่อนแนะนำ model ค่ะ จากนั้นต้องประกาศข้อมูลนี้ก่อน
แก้ source, spawn worker หรือเรียก `agy` ค่ะ

```text
R3 session declaration
Phase: R3.x
Main Sunday model/effort: <model + effort>
Gate/escalation model: <model + effort + trigger>
agy model/tier: <model or not needed + bounded task>
Why this routing: <risk/work shape>
Allowed scope: <files/actions/data boundary>
Approval state: <planning only / implementation approved / external action approved>
```

หากผู้ใช้ยังไม่ได้เลือก model ให้ Sunday แนะนำค่าจากตารางนี้และอธิบาย tradeoff
ก่อนเสนอ execution plan ค่ะ การแนะนำ model ไม่ถือเป็น approval สำหรับ source
write, live mutation, commit, PR, merge หรือ milestone transition ค่ะ

## 8. Official references

- [OpenAI Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents.md) ค่ะ
- [OpenAI Codex models](https://learn.chatgpt.com/docs/models.md) ค่ะ
- [OpenAI latest GPT-5.6 model guidance](https://developers.openai.com/api/docs/guides/latest-model.md) ค่ะ
- [OpenAI Codex CLI commands](https://learn.chatgpt.com/docs/developer-commands.md?surface=cli) ค่ะ
- [OpenAI Codex non-interactive JSON usage](https://learn.chatgpt.com/docs/non-interactive-mode.md) ค่ะ
- [OpenAI Codex analytics](https://learn.chatgpt.com/docs/enterprise/analytics-api.md) ค่ะ
- [Antigravity Model Quotas](https://antigravity.google/docs/cli/commands/usage) ค่ะ
- [Antigravity statusline fields](https://antigravity.google/docs/cli-statusline) ค่ะ
- [Antigravity plans](https://antigravity.google/docs/plans) ค่ะ
- [Antigravity SDK observability](https://antigravity.google/docs/sdk-overview) ค่ะ
