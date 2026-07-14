# R2 — Guarded Upload and Download Acceptance

Owner: Sunday ค่ะ

R2 is complete only when every applicable checkbox below is backed by evidence
from one exact candidate HEADค่ะ Mock, compile, emulator, native runtime, and
live evidence must remain explicitly distinguishedค่ะ

Current status: `IMPLEMENTATION CANDIDATE — LIVE/CI PENDING` ค่ะ The checklist
records the completed offline contract separately from the exact disposable
fixture, live-native, platform-CI, PR-ready, and merge gates that remain openค่ะ
Evidence and external blockers are recorded in [RESULTS.md](RESULTS.md)ค่ะ

## Gate 0 — Baseline and contract

- [x] R1 is merged into `main`, post-merge checks pass, and the R2 branch starts
  from that merge commitค่ะ
- [x] R2 scope, non-goals, Drive mutation allowlist, retry taxonomy, content
  policy, size limits, platform claims, and fault matrix are frozenค่ะ
- [ ] The exact disposable Drive test account/root and two disposable local
  Vaults are recorded without exposing credentials or personal pathsค่ะ
- [x] Dependency and schema migration plans receive security/data-loss reviewค่ะ

## Gate 1 — Native authorization and remote boundary

- [x] Desktop and Android request only the exact restricted full Drive scope
  required for Existing Vault transferค่ะ
- [x] Scope upgrade requires explicit re-consent and re-verifies the exact
  account/root binding before transferค่ะ
- [x] Credentials and bearer-like resumable session URIs never enter the
  WebView, SQLite, logs, serialized errors, or durable historyค่ะ
- [x] Provider origins are pinned, redirects are rejected, response metadata is
  bounded, and streamed bodies have explicit byte limitsค่ะ
- [x] Captured requests prove only allowlisted metadata/media GET, create or
  resumable-init POST, and resumable-session PUT operations occurค่ะ
- [x] No DELETE, Trash, rename, move, permission mutation, generic request API,
  or existing-different-content update is reachable in R2ค่ะ
- [x] Exact account, root ancestry, parent ID, file ID, operation marker, remote
  revision, size, and digest checks fail closed when mismatchedค่ะ

## Gate 2 — Durable transfer state

- [x] Schema v2 migrates transactionally to the exact v3 schema without losing
  binding, cursor, queue, history, or remote metadata evidenceค่ะ
- [x] Newer, negative, partial, malformed, or constraint-weakened schemas are
  preserved and rejected without automatic repairค่ะ
- [x] Queue evidence contains direction, exact identities, expected revisions,
  SHA-256, byte length, MIME class, operation marker, stage/base reference,
  durable phase, retry metadata, and only redacted error codesค่ะ
- [x] Credentials, resumable session URLs, provider bodies, note bodies,
  attachment bodies, and ambient paths are absent from SQLiteค่ะ
- [x] Claim, retry schedule, `AuthRequired`, `NeedsReconcile`, verified
  completion, base publication, tombstone, and history transitions are atomicค่ะ
- [x] Restart converts unknown running work to `NeedsReconcile` and never blindly
  replays a side effectค่ะ

## Gate 3 — Private staging and guarded local publication

- [x] Markdown, zero-byte files, Unicode paths, and binary attachments stream
  through bounded descriptor/native capabilitiesค่ะ
- [x] Downloaded bytes are staged under a private per-Vault root, fsynced,
  length/hash verified, and rechecked against remote metadata before publishค่ะ
- [x] Local publish is create-no-replaceค่ะ Existing same-byte targets are
  verified no-ops; any existing different/stale target becomes
  `NeedsReconcile` without replacementค่ะ
- [x] Publication outcome is verified by byte-for-byte readback; unsupported
  directory durability or unknown publication is reported truthfullyค่ะ
- [x] Base objects are immutable, content-addressed, private, and referenced only
  after durable publicationค่ะ
- [x] Crash, disk-full, cancel, malformed path, symlink/reparse substitution,
  protected path, and stale-session cases preserve evidence and fail closedค่ะ
- [x] Watcher/SAF notifications are coalesced hints, startup reconciliation is
  bounded, and self-write echoes cannot form upload/download loopsค่ะ

## Gate 4 — Upload and download orchestration

- [x] New Markdown and binary objects upload with exact parent, operation marker,
  SHA-256, length, and verified final remote ID/revisionค่ะ
- [x] Resumable upload validates session origin, chunk alignment, advancing
  ranges, total length, status queries, expiry, and final metadataค่ะ
- [x] Lost final responses reconcile exact operation marker/hash/size before any
  retry and never create duplicate remote objectsค่ะ
- [x] Existing same-byte remote objects complete as verified no-opsค่ะ Existing
  different bytes, duplicate paths, or ambiguous ancestry become
  `NeedsReconcile` without mutationค่ะ
- [x] Blob downloads use exact remote IDs and reject Google Workspace native
  MIME/export ambiguity rather than transforming content silentlyค่ะ
- [x] `401`, permission/quota `403`, `404`, `410`, `429 Retry-After`, transient
  `5xx`, timeout, redirect, offline, malformed metadata, and hash mismatch follow
  the frozen retry policyค่ะ
- [x] A single owned worker per Vault releases locks around network/large I/O,
  serializes credential refresh, and suppresses stale-session resultsค่ะ
- [x] Cursor advancement occurs only after verified remote completion or guarded
  local commit and never after a partial/unknown transferค่ะ

## Gate 5 — Deterministic fault and regression matrix

- [x] Upload fault injection covers enqueue, claim, pre/post session initiation,
  every chunk boundary, status query, final bytes accepted/response lost, remote
  verification, base publication, and completion commitค่ะ
- [x] Download fault injection covers request start, mid-stream, staged fsync,
  hash verification, local `Applying`, publish, readback, local commit, base
  publication, completion commit, and pre-cursor advancementค่ะ
- [x] Exact retries are idempotent and mismatched operation-ID reuse is rejectedค่ะ
- [x] Stale local or remote revisions, concurrent session switches, duplicate
  paths, and restarted workers preserve both sides without silent overwriteค่ะ
- [x] R1 read-only behavior, local Vault safety, recovery, mutations, snapshots,
  auth, and frontend suites remain greenค่ะ

## Gate 6 — Offline quality and security

- [x] `pnpm quality:r2:offline` passes frontend typecheck/tests/build plus Rust
  format, strict Clippy, unit, integration, migration, fault, and doc testsค่ะ
- [x] Quality CI covers app-service, core, private-fs, snapshots, sync-engine,
  drive, transfer, desktop-auth, Google auth, private-root, vault-saf, and Tauriค่ะ
- [x] Static and captured-request audits prove the exact R2 method/endpoint
  allowlist and absence of DELETE/Trash/rename/move/permission mutationค่ะ
- [x] Secret/content/path audits find no token, session URI, provider body,
  content body, or ambient Vault path in frontend DTOs, logs, or SQLiteค่ะ
- [x] Production dependency trees contain no `drive-sync-spike` and dependency
  review finds no unresolved high-risk additionค่ะ
- [x] No unresolved P0/P1, data-loss, token-leak, or silent-overwrite finding
  remainsค่ะ

## Gate 7 — Native and platform acceptance

- [ ] A fresh macOS app completes disposable Local Vault A → exact Drive root →
  Local Vault B for Markdown, Unicode, zero-byte, and binary >5 MiB content with
  a byte-exact recursive manifestค่ะ
- [ ] macOS restart during upload and download, offline pause/resume, auth expiry,
  credential restoration, and idempotent disconnect preserve durable truthค่ะ
- [ ] Android API 36 emulator installs a fresh aligned APK and completes the
  supported SAF Markdown/binary round trip, cold restart, offline, and auth
  reacquisition scenariosค่ะ
- [ ] Android evidence remains labeled emulator-only; physical-device acceptance
  remains R7 of the product roadmapค่ะ
- [ ] Ubuntu AppImage and Windows NSIS build/test/package jobs pass on the same
  candidate HEAD without being mislabeled as native UI acceptanceค่ะ

## Gate 8 — Final integrated gate

- [ ] The live disposable round trip, deterministic offline aggregate, platform
  CI, security review, diff review, documentation review, and scope-drift review
  all pass on one exact candidate HEADค่ะ
- [ ] Evidence records HEAD, dirty state, environment, commands/jobs, outcomes,
  classifications, and deliberately untested behaviorค่ะ
- [ ] `PROJECT_PLAN.md`, `SESSION_HANDOFF.md`, `docs/sync/RESULTS.md`, README as
  applicable, and `CHANGELOG.md` agree on the R2 outcome and remaining R3 workค่ะ
- [ ] The R2 PR is ready, checks are green, final review finds no blocker, and the
  approved merge completesค่ะ

R3 must not start until this gate is complete and the roadmap transition is
separately approvedค่ะ
