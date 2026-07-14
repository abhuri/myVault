# R1 — Native Auth + Read-only Existing Drive Binding Acceptance

Owner: Sunday ค่ะ

R1 proves the native authorization, exact account/root binding, production
read-only Drive adapter, durable metadata scan, and Tauri/UI preview pathค่ะ It
does not authorize upload, download-to-Vault, rename, move, Trash, conflict
handling, or continuous background Syncค่ะ

## Evidence contract

Every checkpoint must record the followingค่ะ

- source HEAD and branchค่ะ
- clean or explicitly documented dirty stateค่ะ
- operating system, architecture, and relevant runtime versionsค่ะ
- exact commands and resultsค่ะ
- whether evidence is unit, mock integration, native runtime, emulator, or live fixtureค่ะ
- deliberately untested behavior and the reasonค่ะ

Agent-reported results are advisory until Sunday reruns the checkpointค่ะ A later
source change invalidates affected evidence and requires rerunning that gateค่ะ

## Gate 0 — Baseline

- [x] PR #24 roadmap baseline merged into `origin/main`ค่ะ
- [x] R1 branch created from merged `origin/main`ค่ะ
- [x] Initial working tree and diff check cleanค่ะ
- [x] R1 DTO, error-code, fixture, and schema contracts frozenค่ะ

## Gate 1 — Native authorization

- [x] Desktop OAuth uses literal loopback, PKCE S256, random state, and a bounded callback waitค่ะ
- [x] Desktop token exchange and refresh use pinned HTTPS endpoints, redirects disabled, and bounded timeoutsค่ะ
- [x] Desktop and Android request only `drive.metadata.readonly` in R1ค่ะ
- [x] Refresh tokens are stored only in the OS credential storeค่ะ
- [x] Access tokens, refresh tokens, authorization codes, and PKCE verifiers have redacted diagnostics and no frontend serializationค่ะ
- [x] Auth success and error DTO serialization contains no token-shaped fieldค่ะ
- [x] Mock exchange, refresh, timeout, denial, malformed response, and cleanup tests passค่ะ
- [x] Sunday reruns Auth fmt, Clippy, unit tests, and mock integration testsค่ะ

## Gate 2 — Production read-only Drive adapter

- [x] Production adapter is isolated from `drive-sync-spike`ค่ะ
- [x] Public runtime surface contains no upload, create, update, Trash, delete, or generic mutation requestค่ะ
- [x] Captured mock HTTP requests contain GET onlyค่ะ
- [x] Google API origin is pinned and cross-origin redirects are rejectedค่ะ
- [x] Response bodies are bounded before deserializationค่ะ
- [x] Provider response bodies and bearer values never enter errors or logsค่ะ
- [x] `about.get` returns a validated provider-stable account permission IDค่ะ
- [x] Exact root lookup rejects wrong ID, trashed items, and non-folder itemsค่ะ
- [x] Folder listing preserves duplicate names by exact file IDค่ะ
- [x] Pagination, Unicode, malformed metadata, 401, 403, 404, 410, timeout, and oversized-response tests passค่ะ
- [x] Sunday reruns adapter fmt, Clippy, tests, and static no-mutation scanค่ะ

## Gate 3 — Exact binding and durable metadata scan

- [x] Binding persists an exact verified `(account_id, root_id)` pairค่ะ
- [x] Same pair is idempotent and wrong account/root/name-only attempts fail closedค่ะ
- [x] Legacy v1 root-only state never guesses an account and requires explicit verificationค่ะ
- [x] Recursive scan uses a durable bounded folder frontierค่ะ
- [x] Folder page data, discovered folders, and the next cursor commit atomicallyค่ะ
- [x] Restart after start-token capture, mid-scan, scan completion, and mid-Changes resumes from the last committed boundaryค่ะ
- [x] Rejected scan or Changes pages do not advance durable stateค่ะ
- [x] Expired or ambiguous cursors enter an explicit rescan-required stateค่ะ
- [x] Duplicate paths remain distinct and appear in a bounded paginated previewค่ะ
- [x] Protected `.obsidian/` and `.trash/` paths never enter normal remote stateค่ะ
- [x] SQLite contains no credential or content bodyค่ะ
- [x] Sunday reruns Sync fmt, Clippy, unit, integration, migration, and restart testsค่ะ

## Gate 4 — Tauri and AppService integration

- [x] `myvault-sync-engine` and the production Drive adapter are Tauri dependenciesค่ะ
- [x] `drive-sync-spike` is not a production app dependencyค่ะ
- [x] Native AppService exposes only a non-serializable trusted Vault contextค่ะ
- [x] Tauri commands accept opaque session/exact remote IDs and never accept tokens or ambient local pathsค่ะ
- [x] Auth and scan operations are serialized per active Vaultค่ะ
- [x] Stale Vault sessions suppress in-flight resultsค่ะ
- [x] Worker failure returns a typed redacted outcome without cursor advancementค่ะ
- [x] Sunday reruns Tauri/AppService tests, fmt, Clippy, and DTO serialization checksค่ะ

## Gate 5 — Read-only UI

- [x] UI clearly labels the connection and preview as read-onlyค่ะ
- [x] Root selection uses exact candidate IDs and requires confirmationค่ะ
- [x] UI displays bounded scan status, preview pagination, and duplicate candidatesค่ะ
- [x] Auth-required, wrong-root, rescan-required, cancelled, empty, and error states are understandableค่ะ
- [x] UI exposes no token, provider body, note body, or absolute local Vault pathค่ะ
- [x] Frontend typecheck, unit tests, and production build passค่ะ
- [x] Sunday completes keyboard-only and compact-window native UI inspectionค่ะ

## Gate 6 — Cleanup and rollback

- [x] Disconnect clears in-memory access materialค่ะ
- [x] Credential deletion is idempotent and a partial cleanup failure is typedค่ะ
- [x] Sync lease is released deterministicallyค่ะ
- [x] Local derived state is unbound or quarantined without touching the Vaultค่ะ
- [x] Cleanup emits no Drive data mutation requestค่ะ
- [x] Restart after cleanup does not silently reuse a wrong account or rootค่ะ
- [x] Sunday reruns cleanup fault tests and inspects the native mock UI stateค่ะ

## Gate 7 — Offline final gate

The deterministic local aggregate is `pnpm quality:r1:offline`ค่ะ It runs the
frontend typecheck, unit tests, and production build, then Rust format, strict
Clippy, and tests for desktop auth, the Google auth plugin host, Sync engine,
the production Drive adapter, and the Tauri applicationค่ะ It deliberately does
not execute OAuth, Drive, credential-store, emulator, native-UI, or other live
acceptance actionsค่ะ

The CI evidence matrix for the exact candidate HEAD isค่ะ

| Evidence | Exact command or job | Classification |
| --- | --- | --- |
| Frontend + R1 Rust aggregate | `pnpm quality:r1:offline` | deterministic offline |
| Full repository quality | `.github/workflows/quality.yml` job `quality` | deterministic offline on Ubuntu |
| Android plugin bridge + APK | `.github/workflows/quality.yml` job `android-compile` | compile/package only; not emulator or physical-device evidence |
| Ubuntu AppImage + Windows NSIS | `.github/workflows/platform-build.yml` job `desktop` | native compile/test/package evidence; not native UI acceptance |
| GET-only production adapter | `Audit R1 static trust boundaries` must return no match, plus captured mock-request testsค่ะ | static canary + mock integration |
| No token-shaped frontend field | `Audit R1 static trust boundaries` must return no match in non-test TypeScript/TSXค่ะ | static boundary canary |

The two exact static commands, also embedded in the quality workflow, areค่ะ

```sh
git grep -n -E '\.(post|put|patch|delete)[[:space:]]*\(|Method::(POST|PUT|PATCH|DELETE)' -- crates/myvault-drive/src
git grep -n -I -E '(access[_-]?token|refresh[_-]?token|authorization[_-]?code|code[_-]?verifier)' -- ':(glob)apps/tauri/src/**/*.ts' ':(glob)apps/tauri/src/**/*.tsx' ':(exclude,glob)apps/tauri/src/**/*.test.ts' ':(exclude,glob)apps/tauri/src/**/*.test.tsx'
```

Both commands must exit `1` because no forbidden match existsค่ะ An exit code
greater than `1` is an audit error, not passing evidenceค่ะ

The static canaries are intentionally narrowค่ะ They detect an obvious trust
boundary regression but do not replace Rust type-level non-serialization tests,
captured HTTP-method assertions, SQLite inspection, or native runtime reviewค่ะ
Gate 7 checkboxes were kept unchecked until Sunday reran or verified every item
on one exact integrated HEADค่ะ Gate 8 remains a separately approved live gate and
must never be marked passed by these offline jobsค่ะ

- [x] All touched Rust crates pass formatting, strict Clippy, tests, and documentation testsค่ะ
- [x] Frontend passes typecheck, tests, and production buildค่ะ
- [x] Android APK build and 16 KB alignment passค่ะ
- [x] Quality and platform workflows cover the new production cratesค่ะ
- [x] Static and captured-request audits prove the production Drive path is GET-onlyค่ะ
- [x] Secret scan, serialized DTO scan, SQLite inspection, diff review, and scope-drift review passค่ะ
- [x] Native macOS mock journey passes on the exact offline-gate HEADค่ะ
- [x] No unresolved P0/P1 or data-loss/token-leak finding remainsค่ะ

### Gate 7 evidence — 2026-07-14

- Candidate source: `f29e0862ae5aa1d9aac2cb849bdf8d0e5e491bf0` on
  `codex/r1-readonly-binding`; the only later change in this evidence update is
  this acceptance recordค่ะ
- Host: macOS 26.5.2 build 25F84, arm64; Rust 1.96.0; Node 24.14.1;
  pnpm 11.7.0; OpenJDK 21.0.10; Android NDK 29.0.13846066 and build-tools
  36.0.0ค่ะ
- `pnpm quality:r1:offline` passed: AppService 16, desktop auth 18, Google auth
  plugin 3, Sync engine 24, Drive adapter 17, Tauri 15, and frontend 38 tests,
  with format, strict Clippy, typecheck, documentation tests, and production
  build passingค่ะ
- `pnpm --dir apps/tauri tauri android build --debug --apk --target aarch64`
  produced the universal debug APK; `zipalign -c -P 16 -v 4` ended with
  `Verification successful`ค่ะ
- A freshly rebuilt macOS debug bundle passed Computer Use inspection for the
  unconfigured read-only capability state, disabled Connect control, compact
  context drawer, and keyboard `Escape` close behaviorค่ะ
- Both documented static canaries exited `1` as required; `cargo tree` contained
  no `drive-sync-spike` production dependency; schema inspection found metadata,
  hashes, revisions, cursors, and IDs but no credential or content-body columnค่ะ
- An independent final audit found no remaining P0/P1 after the session-switch
  credential isolation and uniform AppService-then-runtime lock ordering fixesค่ะ
- Deliberately not exercised offline: live OAuth/Drive, the real OS keyring
  round-trip test, Android emulator/physical-device UI, and remote GitHub jobsค่ะ
  They remain Gate 8 evidence and require the separately approved live setupค่ะ

## Gate 8 — Live read-only acceptance

This gate requires separate approval after Gate 7ค่ะ User actions and external
configuration should be batched as close to this gate as possibleค่ะ

- [ ] Exact non-trashed disposable or explicitly allowlisted test root is prepared outside the R1 runtimeค่ะ
- [ ] Native macOS OAuth opens in the system browser with the expected read-only scopeค่ะ
- [ ] Account discovery and exact root binding passค่ะ
- [ ] Initial scan, restart/resume, duplicate preview, and Changes drain passค่ะ
- [ ] Wrong-account and wrong-root attempts fail closedค่ะ
- [ ] Native credential-store restart and idempotent disconnect passค่ะ
- [ ] Captured Drive runtime evidence contains no mutation requestค่ะ
- [ ] No personal Vault or unrelated Drive item is accessedค่ะ
- [ ] Android compile/emulator evidence passes and remains labeled non-physicalค่ะ
- [ ] Quality, Android, Ubuntu, and Windows checks pass on the same source HEADค่ะ
- [ ] Sunday performs final security, diff, documentation, and R1 exit-gate reviewค่ะ

R2 must not start until every applicable R1 item passes and the milestone
transition receives explicit approvalค่ะ
