# R3 — AI Worker Usage and Efficiency Ledger

Owner: Sunday ค่ะ

Status: `MEASUREMENT CONTRACT PREPARED — R3 RUNS NOT STARTED` ค่ะ

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
| _No R3 runs yet_ | — | — | — | — | — | R3 implementation not activated ค่ะ |

Smoke test และ planning pilots ที่ทำก่อน R3 activation เป็น methodology evidence
เท่านั้นค่ะ ไม่ถูกนับเป็น R3 earned work และไม่มี provider-supported exact token
record ค่ะ

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

## 7. Model selection checkpoint

ก่อนเริ่มแต่ละ R3.x phase ให้ตรวจ model availability ปัจจุบันค่ะ Model catalog
เปลี่ยนได้และห้ามใช้ชื่อรุ่นจากเอกสารนี้อย่าง blind หาก runtime ไม่รองรับค่ะ

- Sol High/Extra High ใช้กับ conflict semantics, integration และ final safety ค่ะ
- Terra Medium/High ใช้ bounded implementation ที่มี owned files ชัดค่ะ
- Terra Low/Medium ใช้ exploration, tests, logs และ read-heavy review ค่ะ
- Luna Low/Medium ใช้ extraction/classification ที่มี output schema ชัดค่ะ
- Gemini 3.5 Flash (Low) ใช้ scan/summary ค่ะ Gemini 3.5 Flash (Medium) ใช้
  matrix/fixtures ค่ะ Gemini 3.5 Flash (High) ใช้ bounded second opinion ที่ยากค่ะ

## 8. Official references

- [OpenAI Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents.md) ค่ะ
- [OpenAI Codex models](https://learn.chatgpt.com/docs/models.md) ค่ะ
- [OpenAI Codex CLI commands](https://learn.chatgpt.com/docs/developer-commands.md?surface=cli) ค่ะ
- [OpenAI Codex non-interactive JSON usage](https://learn.chatgpt.com/docs/non-interactive-mode.md) ค่ะ
- [OpenAI Codex analytics](https://learn.chatgpt.com/docs/enterprise/analytics-api.md) ค่ะ
- [Antigravity Model Quotas](https://antigravity.google/docs/cli/commands/usage) ค่ะ
- [Antigravity statusline fields](https://antigravity.google/docs/cli-statusline) ค่ะ
- [Antigravity plans](https://antigravity.google/docs/plans) ค่ะ
- [Antigravity SDK observability](https://antigravity.google/docs/sdk-overview) ค่ะ
