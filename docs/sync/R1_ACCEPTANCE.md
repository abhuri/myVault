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
- [ ] R1 DTO, error-code, fixture, and schema contracts frozenค่ะ

## Gate 1 — Native authorization

- [ ] Desktop OAuth uses literal loopback, PKCE S256, random state, and a bounded callback waitค่ะ
- [ ] Desktop token exchange and refresh use pinned HTTPS endpoints, redirects disabled, and bounded timeoutsค่ะ
- [ ] Desktop and Android request only `drive.metadata.readonly` in R1ค่ะ
- [ ] Refresh tokens are stored only in the OS credential storeค่ะ
- [ ] Access tokens, refresh tokens, authorization codes, and PKCE verifiers have redacted diagnostics and no frontend serializationค่ะ
- [ ] Auth success and error DTO serialization contains no token-shaped fieldค่ะ
- [ ] Mock exchange, refresh, timeout, denial, malformed response, and cleanup tests passค่ะ
- [ ] Sunday reruns Auth fmt, Clippy, unit tests, and mock integration testsค่ะ

## Gate 2 — Production read-only Drive adapter

- [ ] Production adapter is isolated from `drive-sync-spike`ค่ะ
- [ ] Public runtime surface contains no upload, create, update, Trash, delete, or generic mutation requestค่ะ
- [ ] Captured mock HTTP requests contain GET onlyค่ะ
- [ ] Google API origin is pinned and cross-origin redirects are rejectedค่ะ
- [ ] Response bodies are bounded before deserializationค่ะ
- [ ] Provider response bodies and bearer values never enter errors or logsค่ะ
- [ ] `about.get` returns a validated provider-stable account permission IDค่ะ
- [ ] Exact root lookup rejects wrong ID, trashed items, and non-folder itemsค่ะ
- [ ] Folder listing preserves duplicate names by exact file IDค่ะ
- [ ] Pagination, Unicode, malformed metadata, 401, 403, 404, 410, timeout, and oversized-response tests passค่ะ
- [ ] Sunday reruns adapter fmt, Clippy, tests, and static no-mutation scanค่ะ

## Gate 3 — Exact binding and durable metadata scan

- [ ] Binding persists an exact verified `(account_id, root_id)` pairค่ะ
- [ ] Same pair is idempotent and wrong account/root/name-only attempts fail closedค่ะ
- [ ] Legacy v1 root-only state never guesses an account and requires explicit verificationค่ะ
- [ ] Recursive scan uses a durable bounded folder frontierค่ะ
- [ ] Folder page data, discovered folders, and the next cursor commit atomicallyค่ะ
- [ ] Restart after start-token capture, mid-scan, scan completion, and mid-Changes resumes from the last committed boundaryค่ะ
- [ ] Rejected scan or Changes pages do not advance durable stateค่ะ
- [ ] Expired or ambiguous cursors enter an explicit rescan-required stateค่ะ
- [ ] Duplicate paths remain distinct and appear in a bounded paginated previewค่ะ
- [ ] Protected `.obsidian/` and `.trash/` paths never enter normal remote stateค่ะ
- [ ] SQLite contains no credential or content bodyค่ะ
- [ ] Sunday reruns Sync fmt, Clippy, unit, integration, migration, and restart testsค่ะ

## Gate 4 — Tauri and AppService integration

- [ ] `myvault-sync-engine` and the production Drive adapter are Tauri dependenciesค่ะ
- [ ] `drive-sync-spike` is not a production app dependencyค่ะ
- [ ] Native AppService exposes only a non-serializable trusted Vault contextค่ะ
- [ ] Tauri commands accept opaque session/exact remote IDs and never accept tokens or ambient local pathsค่ะ
- [ ] Auth and scan operations are serialized per active Vaultค่ะ
- [ ] Stale Vault sessions suppress in-flight resultsค่ะ
- [ ] Worker failure returns a typed redacted outcome without cursor advancementค่ะ
- [ ] Sunday reruns Tauri/AppService tests, fmt, Clippy, and DTO serialization checksค่ะ

## Gate 5 — Read-only UI

- [ ] UI clearly labels the connection and preview as read-onlyค่ะ
- [ ] Root selection uses exact candidate IDs and requires confirmationค่ะ
- [ ] UI displays bounded scan status, preview pagination, and duplicate candidatesค่ะ
- [ ] Auth-required, wrong-root, rescan-required, cancelled, empty, and error states are understandableค่ะ
- [ ] UI exposes no token, provider body, note body, or absolute local Vault pathค่ะ
- [ ] Frontend typecheck, unit tests, and production build passค่ะ
- [ ] Sunday completes keyboard-only and compact-window native UI inspectionค่ะ

## Gate 6 — Cleanup and rollback

- [ ] Disconnect clears in-memory access materialค่ะ
- [ ] Credential deletion is idempotent and a partial cleanup failure is typedค่ะ
- [ ] Sync lease is released deterministicallyค่ะ
- [ ] Local derived state is unbound or quarantined without touching the Vaultค่ะ
- [ ] Cleanup emits no Drive data mutation requestค่ะ
- [ ] Restart after cleanup does not silently reuse a wrong account or rootค่ะ
- [ ] Sunday reruns cleanup fault tests and inspects the native mock UI stateค่ะ

## Gate 7 — Offline final gate

- [ ] All touched Rust crates pass formatting, strict Clippy, tests, and documentation testsค่ะ
- [ ] Frontend passes typecheck, tests, and production buildค่ะ
- [ ] Android APK build and 16 KB alignment passค่ะ
- [ ] Quality and platform workflows cover the new production cratesค่ะ
- [ ] Static and captured-request audits prove the production Drive path is GET-onlyค่ะ
- [ ] Secret scan, serialized DTO scan, SQLite inspection, diff review, and scope-drift review passค่ะ
- [ ] Native macOS mock journey passes on the exact offline-gate HEADค่ะ
- [ ] No unresolved P0/P1 or data-loss/token-leak finding remainsค่ะ

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
