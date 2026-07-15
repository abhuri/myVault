# R2 — Guarded Transfer Implementation Plan

Owner: Sunday ค่ะ

Activated from post-R1 `main` commit `681271ae497de7b0098e74fbfa9a0a74d1125442`
after one-time approval from คุณโอ on 2026-07-14 Asia/Bangkok ค่ะ

## Outcome

R2 delivers byte-verified, restart-safe transfer of Markdown and binary blob
content between two disposable local Vaults through one exact Google Drive rootค่ะ
Every transfer either reaches a verified durable completion, remains retry-safe,
or stops at `NeedsReconcile` without silently overwriting another revisionค่ะ

## Safety boundary

- Only the approved Google test account and one exact disposable R2 Drive root
  may be usedค่ะ Every remote request must re-establish the bound account and
  prove the target parent/file remains below that rootค่ะ
- The R2 runtime requests the exact restricted full Drive scope already
  configured for the personal test projectค่ะ Credentials remain native-only;
  refresh tokens remain in the OS credential store and access tokens remain in
  memoryค่ะ
- Normal transfer accepts Markdown and Drive blob files onlyค่ะ Google Workspace
  native MIME types, shortcuts, duplicate portable paths, protected paths, and
  ambiguous ancestry stop at a typed non-destructive outcomeค่ะ
- R2 may create files below a verified existing parent, initiate resumable
  uploads, upload chunks, query upload status, and download exact blob IDsค่ะ
- Creating or restructuring remote folder hierarchies remains a guarded R3
  mutation; R2 never infers a parent from a display pathค่ะ
- R2 does not rename, move, Trash, permanently delete, change permissions, or
  mutate content/metadata of an existing remote object with different bytesค่ะ
  Existing same-byte objects are verified no-ops; differing bytes become
  `NeedsReconcile` for R3ค่ะ
- Guarded local publication in R2 is create-no-replaceค่ะ An existing local
  target is either verified as the same bytes or becomes `NeedsReconcile`;
  cross-process/SAF replacement is deferred because neither platform can prove
  an atomic compare-and-swap without risking a silent overwriteค่ะ
- No personal Vault or unrelated Drive item is opened or content-readค่ะ

## Architecture

```text
Native OAuth provider
        │ fresh full-Drive access token, memory only
        ▼
myvault-drive transfer capability ── metadata/streaming operations ──► Drive
        ▲
        │ typed operations and redacted outcomes
myvault-transfer worker
        │
        ├── myvault-sync-engine schema/queue/cursor transactions
        ├── private staging and immutable base-object store
        └── guarded local replica capability (desktop or Android SAF)
```

`myvault-transfer` is Tauri-free and owns orchestration, fault boundaries,
retry classification, and completion rulesค่ะ `myvault-drive` owns the narrow
provider protocolค่ะ `myvault-sync-engine` owns durable operational truth but
never stores credentials or content bodiesค่ะ AppService/platform adapters own
local capabilities; the WebView sees only opaque operation IDs and redacted
statusค่ะ

## Durable transfer contract

### Frozen implementation bounds

- Desktop accepts one transfer payload up to 512 MiB and streams through
  descriptor-backed capabilitiesค่ะ
- Android SAF accepts one transfer payload up to 16 MiBค่ะ The Android bridge
  moves bounded 192 KiB chunks through a stateful provider read session rather
  than one whole-buffer Base64 messageค่ะ The session holds one provider stream
  for O(n) traversal, binds the exact root/path/document identity, and isolates
  its owner transcript from foreign or malformed contendersค่ะ Unknown provider
  sizes remain unknown until bounded traversal establishes the actual byte
  lengthค่ะ
- Resumable upload chunks are 8 MiB, one guarded run processes at most 1,000
  operations, and one incremental drain processes at most 100 Changes pagesค่ะ
- A run that reaches any bound stops truthfully with durable evidence; it never
  widens a limit or advances a cursor past unfinished workค่ะ

Schema v3 adds enough evidence to reconcile an interrupted transfer without
guessingค่ะ A durable transfer records the following before network or local
publication side effectsค่ะ

- operation UUID and transfer directionค่ะ
- exact portable path, remote parent ID, optional exact remote file ID, and
  immutable intended display nameค่ะ
- expected local revision and expected remote revision when applicableค่ะ
- canonical SHA-256 digest, byte length, and MIME classificationค่ะ
- opaque application operation marker used to identify a create retryค่ะ
- durable phase, attempt count, next-attempt time, and redacted error codeค่ะ
- private staging/base-object reference containing no ambient pathค่ะ

Resumable session URIs are bearer-like capabilities and must not enter SQLite,
logs, frontend DTOs, or durable historyค่ะ A process restart abandons an
incomplete session and reconciles by exact parent, operation marker, hash, and
size before deciding whether a new upload is safeค่ะ

Base objects and staged payloads live as descriptor-relative files under the
private per-Vault app-data rootค่ะ They are immutable once published, use
content-addressed names, are fsynced before reference publication, and are never
stored inside SQLite or the Vaultค่ะ Orphan cleanup is evidence-preserving and
bounded; ambiguous objects are retained and fail closed rather than being
deleted or described as quarantined when no quarantine mechanism existsค่ะ

## Transfer state machine

```text
Pending
  └─claim transaction─► Running
       ├─verified completion transaction─► Completed
       ├─verified absence/retry decision─► RetryScheduled
       ├─auth unavailable─► AuthRequired
       └─crash/unknown/stale/ambiguous─► NeedsReconcile
```

Upload completion commits the exact remote ID/revision/hash, base reference,
queue tombstone, and redacted history atomicallyค่ะ Download completion commits
the exact newly-created local revision/hash, base reference, queue tombstone, and redacted
history only after guarded local publication and readback verificationค่ะ A
Changes cursor cannot advance until every declared local mutation is committed
and every remote transfer it depends on is verified completeค่ะ

## Retry contract

- `401` permits one serialized credential refresh, then becomes `AuthRequired`ค่ะ
- Permission `403` fails closedค่ะ Quota/rate classifications use typed redacted
  codes and never include provider bodiesค่ะ
- `429` honors a valid bounded `Retry-After`; otherwise exponential backoff with
  deterministic-testable jitter is usedค่ะ
- Transient `5xx`, connection loss, and timeout retry only after reconciling any
  potentially completed side effectค่ะ
- A malformed response, redirect, origin change, range regression, hash
  mismatch, stale revision, duplicate path, or ambiguous ancestry never retries
  blindlyค่ะ
- Offline state pauses work without incrementing destructive retries and resumes
  from durable evidenceค่ะ

## Local observation contract

Filesystem/SAF notifications are hints rather than truthค่ะ Startup, reconnect,
and coalesced watcher triggers run a bounded inventory comparison against the
durable base metadataค่ะ Application-owned writes enqueue directly and watcher
echoes are suppressed by operation/revision fingerprintsค่ะ Protected
`.obsidian/` and `.trash/` paths never enter the queueค่ะ

## Parallel work ownership

- Lane A owns `crates/myvault-sync-engine/**` and its migration, queue, property,
  and fault testsค่ะ
- Lane B owns `crates/myvault-drive/**`, `crates/desktop-auth/**`,
  `crates/tauri-plugin-google-auth/**`, and Google authorization Android codeค่ะ
- Lane C owns local replica/private staging/platform implementation under
  `crates/myvault-core/**`, `crates/myvault-app-service/**`,
  `crates/tauri-plugin-private-root/**`, and
  `crates/tauri-plugin-vault-saf/**`ค่ะ
- The main integrator owns `crates/myvault-transfer/**`, `apps/tauri/**`, root
  manifests/scripts, lockfiles, workflows, and R2 evidenceค่ะ

Lanes use explicit file ownership whether the Codex surface provides isolated
worktrees or a shared workspaceค่ะ Shared-workspace agents must not edit the
same file concurrently; the main integrator owns manifests, lockfiles, conflict
resolution, and final commitsค่ะ Final integration and every release gate run on
one exact source HEADค่ะ

## Codex execution profile

Official Codex guidance was reviewed on 2026-07-14 before splitting the R2
workค่ะ The main Sunday agent uses Sol High because R2 crosses OAuth, Drive,
durable state, filesystem/SAF, platform, and data-loss boundaries that require
open-ended reasoning and final integration judgmentค่ะ Terra Medium is the
default recommendation for bounded read-heavy investigation, test execution,
and narrow review lanes; Terra High is appropriate when a bounded security
audit needs deeper reasoningค่ะ

Subagents are used only for independent, clearly owned work because parallel
agents consume more aggregate tokens and shared-file writes increase integration
riskค่ะ The current spawn surface did not expose a per-call model selector, so
the evidence must not claim that a particular child model was pinned when it was
not observableค่ะ A future Codex setup may pin model and reasoning in custom
agent configuration when that control is availableค่ะ

References: [Codex subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents.md),
[Codex models](https://learn.chatgpt.com/docs/models.md), and
[long-running work](https://learn.chatgpt.com/docs/long-running-work.md)ค่ะ

## Integration order

1. Freeze types, schema v3, state transitions, retry taxonomy, and fault pointsค่ะ
2. Integrate schema migration and durable transfer evidenceค่ะ
3. Integrate local inventory/enqueue and private staging/base objectsค่ะ
4. Integrate Drive create/resumable upload/reconcile and blob downloadค่ะ
5. Integrate the single owned worker per Vault without holding application locks
   across network or large local I/Oค่ะ
6. Integrate minimal redacted status and recovery actionsค่ะ Full control-plane
   UI remains R4ค่ะ
7. Run deterministic offline gates, platform builds, live disposable round trip,
   final audit, PR, and mergeค่ะ

## Stop conditions

The one-time approval covers all in-scope implementation, tests, browser OAuth,
emulator work, CI, commits, PR, and mergeค่ะ Sunday stops for new authority only
if work would access personal/unallowlisted data, expand OAuth users/scopes,
perform R3 remote mutations, change the locked exit gate, require credentials or
2FA from the user, or reveal a confirmed P0 data-loss/security issueค่ะ
