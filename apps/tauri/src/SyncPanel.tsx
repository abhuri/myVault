import { useEffect, useReducer, useRef, useState } from "react";
import {
  initialSyncState,
  safeSyncFailure,
  scanCanContinue,
  syncApi,
  syncReducer,
  type FolderCandidate,
  type SyncBusy,
  type SyncStatus,
} from "./sync";

const PREVIEW_PAGE_SIZE = 50;
const MAX_FOREGROUND_SCAN_STEPS = 500;

type FolderTrailItem = { id: string | null; name: string };
type BindTarget = { accountId: string; rootId: string; rootName: string };

function StateValue({ active, children }: { active: boolean; children: string }) {
  return <span className={active ? "sync-state yes" : "sync-state"}>{children}</span>;
}

export function SyncPanel({ sessionId, onBusyChange }: { sessionId: string; onBusyChange: (busy: boolean) => void }) {
  const [state, dispatch] = useReducer(syncReducer, sessionId, initialSyncState);
  const [trail, setTrail] = useState<FolderTrailItem[]>([{ id: null, name: "Drive" }]);
  const [cancelling, setCancelling] = useState(false);
  const [bindTarget, setBindTarget] = useState<BindTarget | null>(null);
  const generation = useRef(0);
  const cancelScan = useRef(false);

  const isCurrent = (expectedGeneration: number) =>
    generation.current === expectedGeneration && state.sessionId === sessionId;

  useEffect(() => {
    onBusyChange(state.busy !== null);
  }, [onBusyChange, state.busy]);

  useEffect(() => () => onBusyChange(false), [onBusyChange]);

  useEffect(() => {
    const requestGeneration = ++generation.current;
    cancelScan.current = true;
    setCancelling(false);
    setBindTarget(null);
    setTrail([{ id: null, name: "Drive" }]);
    dispatch({ type: "reset", sessionId });
    void syncApi.status(sessionId)
      .then((status) => {
        if (generation.current === requestGeneration) dispatch({ type: "status", sessionId, status });
      })
      .catch((reason) => {
        if (generation.current === requestGeneration) {
          dispatch({ type: "failure", sessionId, message: safeSyncFailure(reason) });
        }
      });
    return () => {
      generation.current += 1;
      cancelScan.current = true;
    };
  }, [sessionId]);

  const runStatusOperation = async (busy: Exclude<SyncBusy, "folders" | "preview" | "scan" | null>, operation: () => Promise<SyncStatus>) => {
    const requestGeneration = generation.current;
    dispatch({ type: "busy", sessionId, busy });
    try {
      const status = await operation();
      if (isCurrent(requestGeneration)) dispatch({ type: "status", sessionId, status });
    } catch (reason) {
      if (isCurrent(requestGeneration)) dispatch({ type: "failure", sessionId, message: safeSyncFailure(reason) });
    }
  };

  const loadFolders = async (
    parentId: string | null,
    pageToken: string | null,
    append: boolean,
    nextTrail?: FolderTrailItem[],
  ) => {
    const requestGeneration = generation.current;
    dispatch({ type: "busy", sessionId, busy: "folders" });
    try {
      const page = await syncApi.listFolders(sessionId, parentId, pageToken);
      if (!isCurrent(requestGeneration)) return;
      dispatch({ type: "folders", sessionId, page, append });
      if (nextTrail) setTrail(nextTrail);
    } catch (reason) {
      if (isCurrent(requestGeneration)) dispatch({ type: "failure", sessionId, message: safeSyncFailure(reason) });
    }
  };

  const bindSelected = async () => {
    const status = state.status;
    const rootId = state.selectedFolderId;
    const folder = state.folders.find((candidate) => candidate.id === rootId);
    if (!status?.accountId || !rootId || !folder) return;
    const requestGeneration = generation.current;
    const target = { accountId: status.accountId, rootId, rootName: folder.name };
    setBindTarget(target);
    try {
      await runStatusOperation("bind", () => syncApi.bindRoot(sessionId, target.accountId, target.rootId));
    } finally {
      if (generation.current === requestGeneration) setBindTarget(null);
    }
  };

  const loadPreview = async (append: boolean) => {
    const requestGeneration = generation.current;
    dispatch({ type: "busy", sessionId, busy: "preview" });
    try {
      const page = await syncApi.preview(
        sessionId,
        append ? state.previewNextAfter : null,
        PREVIEW_PAGE_SIZE,
      );
      if (isCurrent(requestGeneration)) dispatch({ type: "preview", sessionId, page, append });
    } catch (reason) {
      if (isCurrent(requestGeneration)) dispatch({ type: "failure", sessionId, message: safeSyncFailure(reason) });
    }
  };

  const runScan = async () => {
    if (!state.status?.connected || !state.status.bound || state.busy) return;
    const requestGeneration = generation.current;
    cancelScan.current = false;
    setCancelling(false);
    dispatch({ type: "busy", sessionId, busy: "scan" });
    try {
      let status = state.status;
      for (let step = 0; step < MAX_FOREGROUND_SCAN_STEPS; step += 1) {
        if (cancelScan.current || !isCurrent(requestGeneration)) break;
        status = await syncApi.scanStep(sessionId);
        if (!isCurrent(requestGeneration)) return;
        dispatch({ type: "status", sessionId, status, keepBusy: true });
        if (!scanCanContinue(status)) break;
        if (step === MAX_FOREGROUND_SCAN_STEPS - 1) {
          throw new Error("foreground scan step limit reached");
        }
      }
      if (!isCurrent(requestGeneration)) return;
      dispatch({ type: "busy", sessionId, busy: null });
      if (!cancelScan.current && status.phase.toLocaleLowerCase("en-US") === "ready") await loadPreview(false);
    } catch (reason) {
      if (isCurrent(requestGeneration)) dispatch({ type: "failure", sessionId, message: safeSyncFailure(reason) });
    } finally {
      if (isCurrent(requestGeneration)) setCancelling(false);
    }
  };

  const disconnect = async () => {
    cancelScan.current = true;
    const requestGeneration = generation.current;
    dispatch({ type: "busy", sessionId, busy: "disconnect" });
    try {
      const status = await syncApi.disconnect(sessionId);
      if (!isCurrent(requestGeneration)) return;
      dispatch({ type: "status", sessionId, status });
      setTrail([{ id: null, name: "Drive" }]);
      dispatch({
        type: "folders",
        sessionId,
        append: false,
        page: { sessionId, parentId: null, folders: [], nextPageToken: null },
      });
      dispatch({
        type: "preview",
        sessionId,
        append: false,
        page: { sessionId, entries: [], nextAfter: null, hasMore: false },
      });
    } catch (reason) {
      if (isCurrent(requestGeneration)) dispatch({ type: "failure", sessionId, message: safeSyncFailure(reason) });
    }
  };

  const status = state.status;
  const selected = state.folders.find((folder) => folder.id === state.selectedFolderId);
  const displayedBindTarget = bindTarget ?? (selected && status?.accountId
    ? { accountId: status.accountId, rootId: selected.id, rootName: selected.name }
    : null);
  const disabled = state.busy !== null;

  return (
    <section className="sync-panel" aria-labelledby="drive-sync-title">
      <header>
        <div>
          <p className="section-label">GOOGLE DRIVE</p>
          <h2 id="drive-sync-title">Guarded sync</h2>
        </div>
        {status?.rescanRequired && <span className="sync-warning">Rescan required</span>}
      </header>
      <p className="sync-scope">
        Verified uploads may create remote files only. Downloads may create local files only when absent. Metadata browsing stays read-only.
      </p>

      {state.error && (
        <div className="sync-error" role="alert">
          <span>{state.error}</span>
          <button type="button" onClick={() => dispatch({ type: "clearError", sessionId })} aria-label="Dismiss Drive metadata error">×</button>
        </div>
      )}

      <dl className="sync-status" aria-live="polite">
        <div><dt>Supported</dt><dd><StateValue active={status?.supported === true}>{status?.supported ? "Yes" : "No"}</StateValue></dd></div>
        <div><dt>Binding</dt><dd><StateValue active={status?.bindingAvailable === true}>{status?.bindingAvailable ? "Available" : "Unavailable"}</StateValue></dd></div>
        <div><dt>Configured</dt><dd><StateValue active={status?.configured === true}>{status?.configured ? "Yes" : "No"}</StateValue></dd></div>
        <div><dt>Connected</dt><dd><StateValue active={status?.connected === true}>{status?.connected ? "Yes" : "No"}</StateValue></dd></div>
        <div><dt>Bound</dt><dd><StateValue active={status?.bound === true}>{status?.bound ? "Yes" : "No"}</StateValue></dd></div>
        <div><dt>Phase</dt><dd>{status?.phase || (state.busy === "status" ? "Checking…" : "Unavailable")}</dd></div>
        <div><dt>Rescan</dt><dd><StateValue active={status?.rescanRequired === false}>{status?.rescanRequired ? "Required" : "No"}</StateValue></dd></div>
      </dl>

      {status && (
        <section className="sync-section" aria-labelledby="transfer-status-title">
          <div className="sync-section-heading">
            <h3 id="transfer-status-title">Transfer status</h3>
            <span>{status.active} active</span>
          </div>
          <dl className="sync-status" aria-live="polite">
            <div><dt>Pending</dt><dd>{status.pending}</dd></div>
            <div><dt>Retry scheduled</dt><dd>{status.retryScheduled}</dd></div>
            <div><dt>Authorization required</dt><dd>{status.authRequired}</dd></div>
            <div><dt>Needs reconcile</dt><dd>{status.needsReconcile}</dd></div>
            <div><dt>Completed</dt><dd>{status.completed}</dd></div>
          </dl>
        </section>
      )}

      {status?.accountId && <p className="sync-identity">Account ID <code>{status.accountId}</code></p>}
      {status?.rootId && <p className="sync-identity">Exact root <strong>{status.rootName ?? "Drive folder"}</strong><code>{status.rootId}</code></p>}

      {status && !status.supported && (
        <p className="sync-capability-note" role="status">Read-only Drive metadata is unavailable on this platform.</p>
      )}

      {status?.supported && !status.bindingAvailable && (
        <p className="sync-capability-note" role="status">
          Metadata authorization and folder browsing are supported, but exact local binding and metadata scans are not available on this platform yet.
        </p>
      )}

      {status?.supported && !status.connected && (
        <button
          className="sync-primary"
          type="button"
          disabled={disabled || !status.configured}
          onClick={() => void runStatusOperation("connect", () => syncApi.connect(sessionId))}
        >
          {state.busy === "connect" ? "Connecting…" : "Connect Google Drive"}
        </button>
      )}

      {status?.supported && status.connected && !status.bound && (
        <section className="sync-section" aria-labelledby="folder-picker-title">
          <div className="sync-section-heading">
            <h3 id="folder-picker-title">{status.bindingAvailable ? "Choose an exact folder" : "Browse Drive folders"}</h3>
            <button type="button" disabled={disabled} onClick={() => void loadFolders(trail[trail.length - 1]?.id ?? null, null, false)}>
              {state.busy === "folders" ? "Loading…" : "List folders"}
            </button>
          </div>
          <nav className="sync-breadcrumbs" aria-label="Drive folder location">
            {trail.map((item, index) => (
              <button
                type="button"
                key={`${item.id ?? "root"}-${index}`}
                disabled={disabled || index === trail.length - 1}
                onClick={() => void loadFolders(item.id, null, false, trail.slice(0, index + 1))}
              >{item.name}</button>
            ))}
          </nav>
          {state.folders.length > 0 ? (
            <ul className="sync-folders">
              {state.folders.map((folder: FolderCandidate) => (
                <li key={folder.id}>
                  <div className="sync-folder-details">
                    {status.bindingAvailable && (
                      <input
                        type="radio"
                        name={`drive-root-${sessionId}`}
                        aria-label={`Select ${folder.name}, exact ID ${folder.id}`}
                        disabled={disabled}
                        checked={state.selectedFolderId === folder.id}
                        onChange={() => dispatch({ type: "selectFolder", sessionId, folderId: folder.id })}
                      />
                    )}
                    <span><strong>{folder.name}</strong><code>{folder.id}</code></span>
                  </div>
                  <button
                    type="button"
                    disabled={disabled}
                    aria-label={`Browse inside ${folder.name}`}
                    onClick={() => void loadFolders(folder.id, null, false, [...trail, { id: folder.id, name: folder.name }])}
                  >Browse</button>
                </li>
              ))}
            </ul>
          ) : <p className="sync-empty">List folders to choose by exact Drive ID.</p>}
          {state.folderNextPageToken && (
            <button className="sync-secondary" type="button" disabled={disabled} onClick={() => void loadFolders(state.folderParentId, state.folderNextPageToken, true)}>
              Load more folders
            </button>
          )}
          {state.folderLimitReached && (
            <p className="sync-limit-note" role="status">Folder results reached the local display limit. Narrow the folder location to continue safely.</p>
          )}
          {status.bindingAvailable && displayedBindTarget && (
            <div className="sync-bind-confirm">
              <p>
                {state.busy === "bind" ? "Binding" : "Bind"} account <code>{displayedBindTarget.accountId}</code> to <strong>{displayedBindTarget.rootName}</strong> using exact ID <code>{displayedBindTarget.rootId}</code>.
              </p>
              <button className="sync-primary" type="button" disabled={disabled || !status.accountId} onClick={() => void bindSelected()}>
                {state.busy === "bind" ? "Binding…" : "Bind exact selected folder"}
              </button>
            </div>
          )}
        </section>
      )}

      {status?.supported && status.bindingAvailable && status.connected && status.bound && (
        <section className="sync-section" aria-labelledby="metadata-scan-title">
          <div className="sync-section-heading">
            <h3 id="metadata-scan-title">Metadata scan</h3>
            <span>{status.phase}</span>
          </div>
          {state.busy === "scan" ? (
            <button className="sync-secondary" type="button" disabled={cancelling} onClick={() => { cancelScan.current = true; setCancelling(true); }}>
              {cancelling ? "Cancelling after this step…" : "Cancel scan"}
            </button>
          ) : (
            <button className="sync-primary" type="button" disabled={disabled} onClick={() => void runScan()}>
              {status.rescanRequired ? "Start fresh metadata scan" : "Scan metadata now"}
            </button>
          )}
          <div className="sync-section-heading sync-preview-heading">
            <h3>Remote metadata preview</h3>
            <button type="button" disabled={disabled} onClick={() => void loadPreview(false)}>
              {state.busy === "preview" ? "Loading…" : "Refresh"}
            </button>
          </div>
          {state.preview.length ? (
            <ul className="sync-preview">
              {state.preview.map((entry) => (
                <li className={entry.pathCollision ? "collision" : ""} key={`${entry.fileId}:${entry.path}`}>
                  <span><strong>{entry.path}</strong><small>{entry.kind}{entry.pathCollision ? " · path collision" : ""}</small></span>
                  <code>{entry.fileId}</code>
                </li>
              ))}
            </ul>
          ) : <p className="sync-empty">No remote metadata preview loaded.</p>}
          {state.previewHasMore && (
            <button className="sync-secondary" type="button" disabled={disabled} onClick={() => void loadPreview(true)}>Load more metadata</button>
          )}
          {state.previewLimitReached && (
            <p className="sync-limit-note" role="status">Metadata preview reached the local display limit. Refresh after narrowing the bound scope.</p>
          )}
        </section>
      )}

      {status?.supported && status.connected && (
        <button className="sync-disconnect" type="button" disabled={disabled} onClick={() => void disconnect()}>
          {state.busy === "disconnect" ? "Disconnecting…" : "Disconnect Google Drive"}
        </button>
      )}
    </section>
  );
}
