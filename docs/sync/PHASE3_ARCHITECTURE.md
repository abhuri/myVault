# Phase 3 — Production Drive Sync Architecture

Updated 2026-07-13 Asia/Bangkokค่ะ

## Boundary

Phase 3 keeps the local Vault as the working copy and Google Drive as a remote copy and exchange pointค่ะ `drive-sync-spike` remains an isolated acceptance harness; production state and orchestration live in `myvault-sync-engine`ค่ะ

The React/WebView layer may request high-level Sync actions and read redacted Sync status, but the Sync control/status boundary must never return an access token, refresh token, authorization code, PKCE verifier, authorization header, note body, or attachment bodyค่ะ Existing guarded Editor/Reader commands remain the separate, authorized path for presenting note contentค่ะ

```text
Native authorization provider
        │ fresh access token, memory only
        ▼
Production Drive adapter ── typed metadata/pages only ──► Sync engine
                                                        │
Local Vault capability ◄── guarded local mutations ─────┤
                                                        │
Private app data      ◄── SQLite state + base objects ───┘
```

## Private State

Sync operational state is stored outside the Vault under a private app-data root bound to one local Vault identifierค่ะ The database contains remote IDs, ancestry, hashes, cursors, queue metadata, retry scheduling, and redacted historyค่ะ It must never contain OAuth tokens, authorization payloads, note bodies, or attachment bodiesค่ะ

The database is durable but not a source of user contentค่ะ If it is corrupt or has an unsupported schema, opening fails closed and preserves the evidenceค่ะ Recovery requires an explicit quarantine/rebuild workflow followed by a full local/remote reconciliation; the engine must never silently discard a queue or advance a cursorค่ะ

`SQLite` uses foreign keys, `journal_mode=DELETE`, `synchronous=FULL`, and a private pre-created database fileค่ะ Bundled SQLite still opens by ambient path, so the held private directory, no-follow pre-creation, exact permissions, and post-open verification reduce accidental path substitution but are not a security boundary against a hostile process running as the same OS userค่ะ

Before opening SQLite, `SyncStore` acquires an exclusive OS-level lease on a private per-Vault lock file and holds it for the store lifetimeค่ะ A second live worker fails closed without reading or mutating queue stateค่ะ Only a later opener that acquires the released lease may classify retained `Running` jobs as interruptedค่ะ Version-zero migration treats a database as new only when it contains no user table, index, view, or trigger, and exact schema validation occurs inside the same transaction before commitค่ะ Negative schema versions are malformed evidence and fail closed without normalizationค่ะ

## Initial Sync Ordering

1. Bind the exact local Vault ID to one exact remote root IDค่ะ
2. Capture `changes.getStartPageToken` before scanningค่ะ
3. Persist the token and enter `Scanning` before requesting the first pageค่ะ
4. Apply each scan page and its next-page token in one local transactionค่ะ
5. After the final scan page, enter `Draining` using the previously captured tokenค่ะ
6. Apply each Changes page and its cursor in one local transactionค่ะ
7. Enter `Ready` and publish `newStartPageToken` only after the final local transaction commitsค่ะ

Restarting repeats at most the last uncommitted remote requestค่ะ Remote entry upserts and removals are idempotent, and the durable cursor never advances ahead of local stateค่ะ

## Durable Queue

Queue operation IDs are opaque UUIDs and exact retries remain idempotent after completionค่ะ Completed jobs persist as non-runnable tombstones, and reusing an operation ID with different content is a collision that fails closedค่ะ Jobs contain paths, expected local revisions, exact remote IDs where the operation targets an existing Drive object, attempt counts, scheduling metadata, and redacted error codes onlyค่ะ

A job may leave `Pending` only through an atomic claimค่ะ After the exclusive Sync lease is acquired on process restart, retained `Running` jobs become `NeedsReconcile`, not `Pending`, because the remote result may be unknownค่ะ Upload retry is allowed only after the production adapter verifies remote parent, name, remote ID candidates, and content hashค่ะ

Queue completion, durable tombstone publication, and redacted history publication occur in one transactionค่ะ Completed tombstones are excluded from runnable queue counts but retained to prevent UUID reuseค่ะ

## Cursor Commit Protocol

Incremental Changes batches declare the exact local mutation IDs required for a pageค่ะ Before touching the Vault, a mutation moves durably from `Pending` to `Applying`; after its guarded local operation succeeds it moves to `Committed`ค่ะ A restart with an `Applying` mutation requires explicit outcome reconciliation, and verified absence is required before resetting it to `Pending`ค่ะ The batch cursor may be committed only when every declared mutation is `Committed`, while abort is allowed only before any mutation leaves `Pending`ค่ะ A crash, unknown mutation, or partial local application leaves the previous durable cursor unchangedค่ะ

## Protected Paths and Conflicts

Portable content paths may include Markdown and attachmentsค่ะ Root `.obsidian/` and `.trash/` paths are rejected from normal Sync stateค่ะ There is no silent last-write-winsค่ะ Three-way merge, conflict copies, delete-versus-edit handling, and remote Trash semantics are Phase 3D work and must preserve both versions whenever safety cannot be provenค่ะ

## Phase 3A Non-goals

- No Google OAuth runtime integrationค่ะ
- No live Google Drive requestค่ะ
- No upload, download, rename, move, Trash, or deleteค่ะ
- No Tauri command or Sync UIค่ะ
- No automatic repair or deletion of corrupt operational evidenceค่ะ
- No Windows Sync runtime claimค่ะ Phase 3A runs the suite on macOS/Linux and keeps a native Windows compile-only gate until private-root provisioning is integratedค่ะ

These capabilities are added only after the foundation contracts and isolated tests passค่ะ
