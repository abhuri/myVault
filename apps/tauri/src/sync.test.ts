import { describe, expect, it } from "vitest";
import {
  canOpenAnotherVault,
  initialSyncState,
  mapFolderPage,
  mapPreviewPage,
  mapSyncStatus,
  MAX_ACCUMULATED_FOLDERS,
  MAX_ACCUMULATED_PREVIEW_ENTRIES,
  safeSyncFailure,
  syncReducer,
} from "./sync";

const STATUS = {
  sessionId: "session-a",
  supported: true,
  bindingAvailable: true,
  configured: true,
  connected: true,
  bound: false,
  phase: "unbound",
  rescanRequired: false,
  active: 2,
  pending: 1,
  retryScheduled: 0,
  authRequired: 0,
  needsReconcile: 1,
  completed: 3,
  accountId: "account_1",
  rootId: null,
  rootName: null,
};

describe("sync response mappers", () => {
  it("requires exact session identity and strips unknown secret-shaped fields", () => {
    const mapped = mapSyncStatus({
      ...STATUS,
      accessToken: "must-not-cross",
      operationId: "operation-secret",
      path: "/private/vault/note.md",
      sessionUri: "https://upload.invalid/session",
      providerBody: "provider-secret",
    }, "session-a");
    expect(mapped).toEqual(STATUS);
    const serialized = JSON.stringify(mapped);
    for (const forbidden of ["must-not-cross", "operation-secret", "/private/", "upload.invalid", "provider-secret"]) {
      expect(serialized).not.toContain(forbidden);
    }
    expect(() => mapSyncStatus({ ...STATUS, sessionId: "session-b" }, "session-a"))
      .toThrow("identity mismatch");
  });

  it("requires explicit platform and exact-binding capabilities", () => {
    expect(mapSyncStatus({ ...STATUS, bindingAvailable: false }, "session-a").bindingAvailable).toBe(false);
    expect(mapSyncStatus({ ...STATUS, supported: false }, "session-a").supported).toBe(false);
    const { supported: _supported, ...missingSupport } = STATUS;
    expect(() => mapSyncStatus(missingSupport, "session-a")).toThrow("platform support");
  });

  it("requires non-negative safe transfer counts", () => {
    expect(mapSyncStatus({ ...STATUS, completed: Number.MAX_SAFE_INTEGER }, "session-a").completed)
      .toBe(Number.MAX_SAFE_INTEGER);
    for (const invalid of [-1, 1.5, Number.MAX_SAFE_INTEGER + 1, "1", null]) {
      expect(() => mapSyncStatus({ ...STATUS, pending: invalid }, "session-a"))
        .toThrow("pending transfer count");
    }
    const { needsReconcile: _needsReconcile, ...missingCount } = STATUS;
    expect(() => mapSyncStatus(missingCount, "session-a"))
      .toThrow("reconciliation-required transfer count");
    expect(() => mapSyncStatus({ ...STATUS, active: 1 }, "session-a"))
      .toThrow("inconsistent");
  });

  it("preserves duplicate folder names by exact id", () => {
    const page = mapFolderPage({
      sessionId: "session-a",
      parentId: null,
      folders: [{ id: "folder_1", name: "Notes" }, { id: "folder_2", name: "Notes" }],
      nextPageToken: "next_1",
    }, "session-a", null, null);
    expect(page.folders.map((folder) => folder.id)).toEqual(["folder_1", "folder_2"]);
  });

  it("rejects folder parent mismatches and replayed page cursors", () => {
    const response = {
      sessionId: "session-a",
      parentId: "folder_2",
      folders: [],
      nextPageToken: "page_2",
    };
    expect(() => mapFolderPage(response, "session-a", "folder_1", "page_1"))
      .toThrow("parent identity mismatch");
    expect(() => mapFolderPage({ ...response, parentId: "folder_1", nextPageToken: "page_1" }, "session-a", "folder_1", "page_1"))
      .toThrow("cursor did not advance");
  });

  it("validates collision-aware preview rows", () => {
    const page = mapPreviewPage({
      sessionId: "session-a",
      entries: [{ fileId: "file_1", path: "Notes/duplicate.md", kind: "file", pathCollision: true }],
      nextAfter: null,
      hasMore: false,
    }, "session-a", null);
    expect(page.entries[0].pathCollision).toBe(true);
    expect(() => mapPreviewPage({ ...page, entries: [{ ...page.entries[0], kind: "note" }] }, "session-a", null))
      .toThrow("kind is invalid");
  });

  it("requires advancing preview continuation cursors", () => {
    const response = { sessionId: "session-a", entries: [], nextAfter: null, hasMore: true };
    expect(() => mapPreviewPage(response, "session-a", null)).toThrow("missing its continuation cursor");
    expect(() => mapPreviewPage({ ...response, nextAfter: "entry_1" }, "session-a", "entry_1"))
      .toThrow("cursor did not advance");
  });
});

describe("sync reducer", () => {
  it("blocks Vault switching while a native Drive operation is active", () => {
    expect(canOpenAnotherVault(true)).toBe(false);
    expect(canOpenAnotherVault(false)).toBe(true);
  });

  it("ignores late actions from a previous Vault session", () => {
    const state = initialSyncState("session-b");
    expect(syncReducer(state, { type: "status", sessionId: "session-a", status: STATUS })).toBe(state);
  });

  it("requires selection of a returned exact folder id", () => {
    const state = syncReducer(initialSyncState("session-a"), {
      type: "folders",
      sessionId: "session-a",
      append: false,
      page: {
        sessionId: "session-a",
        parentId: null,
        folders: [{ id: "folder_1", name: "Notes" }],
        nextPageToken: null,
      },
    });
    expect(syncReducer(state, { type: "selectFolder", sessionId: "session-a", folderId: "Notes" }))
      .toBe(state);
    expect(syncReducer(state, { type: "selectFolder", sessionId: "session-a", folderId: "folder_1" }).selectedFolderId)
      .toBe("folder_1");
  });

  it("resets all Drive-derived state when the Vault session changes", () => {
    const populated = { ...initialSyncState("session-a"), status: STATUS, selectedFolderId: "folder_1" };
    expect(syncReducer(populated, { type: "reset", sessionId: "session-b" }))
      .toEqual(initialSyncState("session-b"));
  });

  it("bounds accumulated folders deterministically", () => {
    const folders = Array.from({ length: MAX_ACCUMULATED_FOLDERS + 1 }, (_, index) => ({
      id: `folder_${index}`,
      name: `Folder ${index}`,
    }));
    const state = syncReducer(initialSyncState("session-a"), {
      type: "folders",
      sessionId: "session-a",
      append: true,
      page: { sessionId: "session-a", parentId: null, folders, nextPageToken: "more" },
    });
    expect(state.folders).toHaveLength(MAX_ACCUMULATED_FOLDERS);
    expect(state.folders[state.folders.length - 1]?.id).toBe(`folder_${MAX_ACCUMULATED_FOLDERS - 1}`);
    expect(state.folderNextPageToken).toBeNull();
    expect(state.folderLimitReached).toBe(true);
  });

  it("deduplicates preview by file id and bounds accumulated state", () => {
    const existing = Array.from({ length: MAX_ACCUMULATED_PREVIEW_ENTRIES }, (_, index) => ({
      fileId: `file_${index}`,
      path: `old/${index}.md`,
      kind: "file" as const,
      pathCollision: false,
    }));
    const initial = syncReducer(initialSyncState("session-a"), {
      type: "preview",
      sessionId: "session-a",
      append: false,
      page: { sessionId: "session-a", entries: existing, nextAfter: "cursor_1", hasMore: true },
    });
    const state = syncReducer(initial, {
      type: "preview",
      sessionId: "session-a",
      append: true,
      page: {
        sessionId: "session-a",
        entries: [
          { fileId: "file_0", path: "new/0.md", kind: "file", pathCollision: false },
          { fileId: "file_extra", path: "new/extra.md", kind: "file", pathCollision: false },
        ],
        nextAfter: "cursor_2",
        hasMore: true,
      },
    });
    expect(state.preview).toHaveLength(MAX_ACCUMULATED_PREVIEW_ENTRIES);
    expect(state.preview[0].path).toBe("new/0.md");
    expect(state.preview.some((entry) => entry.fileId === "file_extra")).toBe(false);
    expect(state.previewHasMore).toBe(false);
    expect(state.previewLimitReached).toBe(true);
  });

  it("never exposes backend error messages", () => {
    expect(safeSyncFailure({ code: "authRequired", message: "Bearer secret" }))
      .toBe("Connect Google Drive before continuing.");
    expect(safeSyncFailure({ message: "Bearer secret" })).not.toContain("secret");
  });

  it("maps exact backend configuration and rescan codes without using backend messages", () => {
    expect(safeSyncFailure({ code: "unconfigured", message: "Bearer secret" }))
      .toBe("Google Drive access is not configured on this device.");
    expect(safeSyncFailure({ code: "rescanRequired", message: "Bearer secret" }))
      .toBe("Drive metadata must be scanned again before the preview can continue.");
  });
});
