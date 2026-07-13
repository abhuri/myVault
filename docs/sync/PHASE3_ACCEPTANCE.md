# Phase 3A — Sync Foundation Acceptance

Updated 2026-07-13 Asia/Bangkokค่ะ

## Safety Gates

- [x] Production sync code is isolated from the Phase 0 fixture harnessค่ะ
- [x] Private SQLite state is outside and disjoint from the Vaultค่ะ
- [x] A Vault ID cannot be rebound silently to a different remote root IDค่ะ
- [x] Initial sync captures a start token before the first scan requestค่ะ
- [x] Scan pages and page tokens commit atomicallyค่ะ
- [x] Changes pages and the durable cursor commit atomicallyค่ะ
- [x] A restart resumes from the last committed scan/change pageค่ะ
- [x] Queue operation IDs remain exact-idempotent after completion and reject mismatched reuse through durable completed tombstonesค่ะ
- [x] Download, Move, and Trash queue jobs require an exact remote file IDค่ะ
- [x] Interrupted running jobs become `NeedsReconcile`ค่ะ
- [x] Queue completion and redacted history commit atomicallyค่ะ
- [x] Incremental cursor publication is blocked until every declared local mutation commitsค่ะ Applying mutations survive restart and require reconciliation before retryค่ะ
- [x] A batch with applying or committed local mutations cannot discard its evidence through abortค่ะ
- [x] Protected `.obsidian/` and `.trash/` paths are rejectedค่ะ
- [x] Database rows contain no OAuth tokens, authorization payloads, note bodies, or attachment bodiesค่ะ
- [x] Newer, malformed, constraint-weakened, or corrupt database evidence fails closed without automatic deletionค่ะ
- [x] Rust formatting, strict Clippy, isolated tests, existing Tauri tests, and secret/diff checks passค่ะ
- [x] Native Linux runs the Sync Foundation suite, while Windows compiles the tests without claiming runtime acceptanceค่ะ

## Mock Scenarios

1. Capture token, scan multiple pages, drain multiple Changes pages, and reach `Ready`ค่ะ
2. Stop after a committed scan page, reopen the database, and resume from the persisted next-page tokenค่ะ
3. Stop after fetching but before committing a page, reopen, and safely request the same page againค่ะ
4. Apply a removed change without relying on parent metadataค่ะ
5. Attempt to commit an incremental cursor with one local mutation missing and confirm the previous cursor remainsค่ะ
6. Reopen with a `Running` upload and confirm it requires reconciliation before retryค่ะ
7. Complete a queue operation, retry it exactly, and confirm its durable tombstone prevents duplicate executionค่ะ
8. Retry a completed operation ID with a different path, revision, or kind and confirm a collisionค่ะ
9. Supply a protected or noncanonical path and confirm no state is writtenค่ะ
10. Open a partial, constraint-weakened, malformed, or newer schema and confirm the database is preserved and rejectedค่ะ
11. Restart with an `Applying` local mutation and confirm retry, abort, and cursor commit are blocked until explicit reconciliationค่ะ

## External Safety

Phase 3A must not read from or write to Google Driveค่ะ Live account access requires a separate approval in Phase 3Bค่ะ
