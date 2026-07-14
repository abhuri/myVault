import { invoke } from "@tauri-apps/api/core";

export type SyncStatus = {
  sessionId: string;
  supported: boolean;
  bindingAvailable: boolean;
  configured: boolean;
  connected: boolean;
  bound: boolean;
  phase: string;
  rescanRequired: boolean;
  active: number;
  pending: number;
  retryScheduled: number;
  authRequired: number;
  needsReconcile: number;
  completed: number;
  accountId: string | null;
  rootId: string | null;
  rootName: string | null;
};

export type FolderCandidate = {
  id: string;
  name: string;
};

export type FolderPage = {
  sessionId: string;
  parentId: string | null;
  folders: FolderCandidate[];
  nextPageToken: string | null;
};

export type PreviewEntry = {
  fileId: string;
  path: string;
  kind: "file" | "folder";
  pathCollision: boolean;
};

export type PreviewPage = {
  sessionId: string;
  entries: PreviewEntry[];
  nextAfter: string | null;
  hasMore: boolean;
};

export type SyncBusy = "status" | "connect" | "folders" | "bind" | "scan" | "preview" | "transfer" | "disconnect" | null;

export type SyncUiState = {
  sessionId: string;
  status: SyncStatus | null;
  folders: FolderCandidate[];
  folderParentId: string | null;
  folderNextPageToken: string | null;
  folderLimitReached: boolean;
  selectedFolderId: string | null;
  preview: PreviewEntry[];
  previewNextAfter: string | null;
  previewHasMore: boolean;
  previewLimitReached: boolean;
  busy: SyncBusy;
  error: string | null;
};

export type SyncAction =
  | { type: "reset"; sessionId: string }
  | { type: "busy"; sessionId: string; busy: SyncBusy }
  | { type: "status"; sessionId: string; status: SyncStatus; keepBusy?: boolean }
  | { type: "folders"; sessionId: string; page: FolderPage; append: boolean }
  | { type: "selectFolder"; sessionId: string; folderId: string }
  | { type: "preview"; sessionId: string; page: PreviewPage; append: boolean }
  | { type: "failure"; sessionId: string; message: string }
  | { type: "clearError"; sessionId: string };

const MAX_TEXT = 4096;
export const MAX_ACCUMULATED_FOLDERS = 2000;
export const MAX_ACCUMULATED_PREVIEW_ENTRIES = 1000;
export const SYNC_BUSY_VAULT_MESSAGE = "Finish or cancel the active Google Drive operation before opening another Vault.";

export function canOpenAnotherVault(syncBusy: boolean): boolean {
  return !syncBusy;
}

function recordOf(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${label} response is invalid`);
  }
  return value as Record<string, unknown>;
}

function textOf(value: unknown, label: string, allowEmpty = false): string {
  if (typeof value !== "string" || value.length > MAX_TEXT || (!allowEmpty && value.length === 0)) {
    throw new Error(`${label} response is invalid`);
  }
  return value;
}

function optionalTextOf(value: unknown, label: string): string | null {
  return value === null || value === undefined ? null : textOf(value, label);
}

function booleanOf(value: unknown, label: string): boolean {
  if (typeof value !== "boolean") throw new Error(`${label} response is invalid`);
  return value;
}

function countOf(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${label} response is invalid`);
  }
  return value;
}

function assertSession(value: unknown, expectedSessionId: string): string {
  const sessionId = textOf(value, "session identity");
  if (sessionId !== expectedSessionId) throw new Error("Sync response identity mismatch");
  return sessionId;
}

export function mapSyncStatus(value: unknown, expectedSessionId: string): SyncStatus {
  const raw = recordOf(value, "Sync status");
  const active = countOf(raw.active, "active transfer count");
  const pending = countOf(raw.pending, "pending transfer count");
  const retryScheduled = countOf(raw.retryScheduled, "scheduled retry count");
  const authRequired = countOf(raw.authRequired, "authorization-required transfer count");
  const needsReconcile = countOf(raw.needsReconcile, "reconciliation-required transfer count");
  if (active < pending + retryScheduled + authRequired + needsReconcile) {
    throw new Error("Transfer status response is inconsistent");
  }
  return {
    sessionId: assertSession(raw.sessionId, expectedSessionId),
    supported: booleanOf(raw.supported, "platform support"),
    bindingAvailable: booleanOf(raw.bindingAvailable, "binding availability"),
    configured: booleanOf(raw.configured, "configured"),
    connected: booleanOf(raw.connected, "connected"),
    bound: booleanOf(raw.bound, "bound"),
    phase: textOf(raw.phase, "phase", true),
    rescanRequired: booleanOf(raw.rescanRequired, "rescan"),
    active,
    pending,
    retryScheduled,
    authRequired,
    needsReconcile,
    completed: countOf(raw.completed, "completed transfer count"),
    accountId: optionalTextOf(raw.accountId, "account id"),
    rootId: optionalTextOf(raw.rootId, "root id"),
    rootName: optionalTextOf(raw.rootName, "root name"),
  };
}

export function mapFolderPage(
  value: unknown,
  expectedSessionId: string,
  expectedParentId: string | null,
  expectedPageToken: string | null,
): FolderPage {
  const raw = recordOf(value, "Folder page");
  if (!Array.isArray(raw.folders) || raw.folders.length > 1000) throw new Error("Folder page response is invalid");
  const seen = new Set<string>();
  const folders = raw.folders.map((value) => {
    const folder = recordOf(value, "Folder");
    const id = textOf(folder.id, "folder id");
    if (seen.has(id)) throw new Error("Folder page contains a duplicate id");
    seen.add(id);
    return { id, name: textOf(folder.name, "folder name") };
  });
  const parentId = optionalTextOf(raw.parentId, "parent id");
  const nextPageToken = optionalTextOf(raw.nextPageToken, "folder page token");
  if (parentId !== expectedParentId) throw new Error("Folder page parent identity mismatch");
  if (expectedPageToken !== null && nextPageToken === expectedPageToken) {
    throw new Error("Folder page cursor did not advance");
  }
  return {
    sessionId: assertSession(raw.sessionId, expectedSessionId),
    parentId,
    folders,
    nextPageToken,
  };
}

export function mapPreviewPage(value: unknown, expectedSessionId: string, expectedAfter: string | null): PreviewPage {
  const raw = recordOf(value, "Preview page");
  if (!Array.isArray(raw.entries) || raw.entries.length > 200) throw new Error("Preview page response is invalid");
  const entries = raw.entries.map((value) => {
    const entry = recordOf(value, "Preview entry");
    const rawKind = textOf(entry.kind, "entry kind");
    if (rawKind !== "file" && rawKind !== "folder") throw new Error("Preview entry kind is invalid");
    const kind: PreviewEntry["kind"] = rawKind;
    return {
      fileId: textOf(entry.fileId, "file id"),
      path: textOf(entry.path, "entry path"),
      kind,
      pathCollision: booleanOf(entry.pathCollision, "path collision"),
    };
  });
  const nextAfter = optionalTextOf(raw.nextAfter, "preview cursor");
  const hasMore = booleanOf(raw.hasMore, "preview continuation");
  if (hasMore && nextAfter === null) throw new Error("Preview response is missing its continuation cursor");
  if (expectedAfter !== null && nextAfter === expectedAfter) throw new Error("Preview cursor did not advance");
  return {
    sessionId: assertSession(raw.sessionId, expectedSessionId),
    entries,
    nextAfter,
    hasMore,
  };
}

export function initialSyncState(sessionId: string): SyncUiState {
  return {
    sessionId,
    status: null,
    folders: [],
    folderParentId: null,
    folderNextPageToken: null,
    folderLimitReached: false,
    selectedFolderId: null,
    preview: [],
    previewNextAfter: null,
    previewHasMore: false,
    previewLimitReached: false,
    busy: "status",
    error: null,
  };
}

export function syncReducer(state: SyncUiState, action: SyncAction): SyncUiState {
  if (action.type === "reset") return initialSyncState(action.sessionId);
  if (action.sessionId !== state.sessionId) return state;
  switch (action.type) {
    case "busy":
      return { ...state, busy: action.busy, error: null };
    case "status":
      return { ...state, status: action.status, busy: action.keepBusy ? state.busy : null, error: null };
    case "folders": {
      const combined = action.append
        ? [...new Map([...state.folders, ...action.page.folders].map((folder) => [folder.id, folder])).values()]
        : action.page.folders;
      const folderLimitReached = combined.length > MAX_ACCUMULATED_FOLDERS;
      return {
        ...state,
        folders: combined.slice(0, MAX_ACCUMULATED_FOLDERS),
        folderParentId: action.page.parentId,
        folderNextPageToken: folderLimitReached ? null : action.page.nextPageToken,
        folderLimitReached,
        selectedFolderId: action.append ? state.selectedFolderId : null,
        busy: null,
        error: null,
      };
    }
    case "selectFolder":
      return state.folders.some((folder) => folder.id === action.folderId)
        ? { ...state, selectedFolderId: action.folderId, error: null }
        : state;
    case "preview": {
      const combined = action.append
        ? [...new Map([...state.preview, ...action.page.entries].map((entry) => [entry.fileId, entry])).values()]
        : [...new Map(action.page.entries.map((entry) => [entry.fileId, entry])).values()];
      const previewLimitReached = combined.length > MAX_ACCUMULATED_PREVIEW_ENTRIES;
      return {
        ...state,
        preview: combined.slice(0, MAX_ACCUMULATED_PREVIEW_ENTRIES),
        previewNextAfter: previewLimitReached ? null : action.page.nextAfter,
        previewHasMore: previewLimitReached ? false : action.page.hasMore,
        previewLimitReached,
        busy: null,
        error: null,
      };
    }
    case "failure":
      return { ...state, busy: null, error: action.message };
    case "clearError":
      return { ...state, error: null };
  }
}

export function safeSyncFailure(reason: unknown): string {
  const raw = typeof reason === "object" && reason !== null ? reason as Record<string, unknown> : {};
  const code = typeof raw.code === "string" ? raw.code : "";
  const messages: Record<string, string> = {
    authRequired: "Connect Google Drive before continuing.",
    bindingMismatch: "The selected account or exact folder no longer matches this Vault.",
    cursorExpired: "Drive metadata changed too far back. A fresh read-only scan is required.",
    cursorAmbiguous: "A Drive move could not be mapped safely. A fresh read-only scan is required.",
    rescanRequired: "Drive metadata must be scanned again before the preview can continue.",
    notConfigured: "Google Drive access is not configured on this device.",
    unconfigured: "Google Drive access is not configured on this device.",
    staleSession: "The active Vault changed. Reopen this panel for the current Vault.",
  };
  return messages[code] ?? "The Google Drive operation could not be completed.";
}

export const syncApi = {
  async status(sessionId: string): Promise<SyncStatus> {
    return mapSyncStatus(await invoke<unknown>("sync_status", { sessionId }), sessionId);
  },
  async connect(sessionId: string): Promise<SyncStatus> {
    await invoke("sync_connect", { sessionId });
    return this.status(sessionId);
  },
  async listFolders(sessionId: string, parentId: string | null, pageToken: string | null): Promise<FolderPage> {
    return mapFolderPage(
      await invoke<unknown>("sync_list_folders", { sessionId, parentId, pageToken }),
      sessionId,
      parentId,
      pageToken,
    );
  },
  async bindRoot(sessionId: string, accountId: string, rootId: string): Promise<SyncStatus> {
    await invoke("sync_bind_root", { sessionId, accountId, rootId });
    return this.status(sessionId);
  },
  async scanStep(sessionId: string): Promise<SyncStatus> {
    await invoke("sync_scan_step", { sessionId });
    return this.status(sessionId);
  },
  async preview(sessionId: string, after: string | null, limit: number): Promise<PreviewPage> {
    return mapPreviewPage(
      await invoke<unknown>("sync_preview", { sessionId, after, limit }),
      sessionId,
      after,
    );
  },
  async runGuarded(sessionId: string): Promise<SyncStatus> {
    return mapSyncStatus(
      await invoke<unknown>("sync_run_guarded", { sessionId }),
      sessionId,
    );
  },
  async disconnect(sessionId: string): Promise<SyncStatus> {
    await invoke("sync_disconnect", { sessionId });
    return this.status(sessionId);
  },
};

export function scanCanContinue(status: SyncStatus): boolean {
  return status.supported
    && status.bindingAvailable
    && status.connected
    && status.bound
    && !status.rescanRequired
    && status.phase.toLocaleLowerCase("en-US") !== "ready";
}
