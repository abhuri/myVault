import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { basicSetup } from "codemirror";
import { markdown } from "@codemirror/lang-markdown";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import Graph from "graphology";
import Sigma from "sigma";
import {
  buildTree,
  extractOutline,
  extractWikiLinks,
  filterEntries,
  matchesDocumentIdentity,
  saveReducer,
  shouldAutosave,
  type ExplorerEntry,
  type SaveState,
  type TreeNode,
} from "./workspace";
import {
  markdownHtml,
  planVaultChange,
  preventReaderAnchorNavigation,
  readerScrollCommand,
  renderMermaidSources,
  VAULT_CHANGE_DEBOUNCE_MS,
} from "./reader";
import { SyncPanel } from "./SyncPanel";
import { canOpenAnotherVault, SYNC_BUSY_VAULT_MESSAGE } from "./sync";
import "./App.css";

type VaultStatus = { active: boolean; sessionId: string | null };
type VaultChoice =
  | { outcome: "activated"; status: VaultStatus }
  | { outcome: "cancelled" };
type ExplorerPage = {
  sessionId: string;
  entries: ExplorerEntry[];
  nextAfter: string | null;
  hasMore: boolean;
  scannedEntries: number;
};
type NoteDto = {
  sessionId: string;
  path: string;
  text: string;
  revisionHex: string;
  byteLen: number;
};
type SaveDto = {
  sessionId: string;
  path: string;
  revisionHex: string;
  byteLen: number;
  durability: "fullySynced" | "directorySyncUnsupported";
};
type AppFailure = { code?: string; message?: string };

const INITIAL_SAVE: SaveState = { phase: "clean", revisionHex: "", byteLen: 0 };

function failureOf(reason: unknown): AppFailure {
  if (typeof reason === "object" && reason !== null) return reason as AppFailure;
  return { message: String(reason) };
}

function noteLabel(path: string): string {
  return path.split("/").pop()?.replace(/\.(?:md|markdown)$/i, "") ?? path;
}

function useMedia(query: string): boolean {
  const [matches, setMatches] = useState(() => window.matchMedia(query).matches);
  useEffect(() => {
    const media = window.matchMedia(query);
    const update = () => setMatches(media.matches);
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, [query]);
  return matches;
}

function trapTab(event: import("react").KeyboardEvent, container: HTMLElement | null) {
  if (event.key !== "Tab" || !container) return;
  const controls = [...container.querySelectorAll<HTMLElement>('button,input,summary,[tabindex]:not([tabindex="-1"])')]
    .filter((element) => !element.hasAttribute("disabled") && element.getAttribute("aria-hidden") !== "true");
  if (!controls.length) return;
  const first = controls[0];
  const last = controls[controls.length - 1];
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}

function Editor({ text, onChange }: { text: string; onChange: (next: string) => void }) {
  const host = useRef<HTMLDivElement>(null);
  const initialText = useRef(text);
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;

  useEffect(() => {
    if (!host.current) return;
    const state = EditorState.create({
      doc: initialText.current,
      extensions: [
        basicSetup,
        history(),
        keymap.of([...defaultKeymap, ...historyKeymap]),
        markdown(),
        EditorView.lineWrapping,
        EditorView.updateListener.of((update) => {
          if (update.docChanged) onChangeRef.current(update.state.doc.toString());
        }),
        EditorView.theme({
          "&": { height: "100%", background: "#0e1116", color: "#dfe6ee" },
          ".cm-content": { padding: "28px 7vw 48px", caretColor: "#b6ceff", maxWidth: "900px", margin: "0 auto" },
          ".cm-gutters": { background: "#0e1116", color: "#596371", border: "none" },
          ".cm-activeLine, .cm-activeLineGutter": { background: "#151a21" },
          ".cm-selectionBackground": { background: "#294064 !important" },
          "&.cm-focused": { outline: "none" },
        }),
      ],
    });
    const view = new EditorView({ state, parent: host.current });
    return () => view.destroy();
  }, []);

  return <div className="editor" ref={host} aria-label="Markdown editor" />;
}

const Reader = memo(function Reader({ text }: { text: string }) {
  const host = useRef<HTMLDivElement>(null);
  const html = useMemo(() => markdownHtml(text), [text]);

  useEffect(() => {
    const reader = host.current;
    if (!reader) return;
    reader.addEventListener("click", preventReaderAnchorNavigation);
    return () => reader.removeEventListener("click", preventReaderAnchorNavigation);
  }, []);

  useEffect(() => {
    let active = true;
    const nodes = [...(host.current?.querySelectorAll<HTMLElement>("pre.mermaid-source") ?? [])];
    if (!nodes.length) return;
    void (async () => {
      try {
        const { default: mermaid } = await import("mermaid");
        mermaid.initialize({ startOnLoad: false, securityLevel: "strict", theme: "dark", htmlLabels: false });
        await renderMermaidSources(nodes, mermaid.render.bind(mermaid), () => active);
      } catch {
        if (active) nodes.filter((node) => node.isConnected).forEach((node) => {
          node.classList.add("render-error");
          node.setAttribute("aria-label", "Mermaid diagram could not be rendered");
        });
      }
    })();
    return () => {
      active = false;
    };
  }, [html]);

  return <article className="reader" ref={host} dangerouslySetInnerHTML={{ __html: html }} />;
});

function Tree({ nodes, selected, onSelect }: { nodes: TreeNode[]; selected?: string; onSelect: (path: string) => void }) {
  return (
    <ul className="tree-list">
      {nodes.map((node) =>
        node.type === "folder" ? (
          <li key={`folder-${node.path}`}>
            <details open>
              <summary><span className="tree-mark">⌄</span>{node.name}</summary>
              <Tree nodes={node.children} selected={selected} onSelect={onSelect} />
            </details>
          </li>
        ) : (
          <li key={node.path}>
            <button className={selected === node.path ? "tree-file selected" : "tree-file"} onClick={() => onSelect(node.path)} type="button">
              <span className="file-mark">M</span><span>{node.name}</span>
            </button>
          </li>
        ),
      )}
    </ul>
  );
}

function KnowledgeGraph({ notes, activePath }: { notes: Record<string, string>; activePath?: string }) {
  const host = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!host.current) return;
    const paths = Object.keys(notes);
    const graph = new Graph();
    paths.forEach((path, index) => {
      const angle = (Math.PI * 2 * index) / Math.max(paths.length, 1);
      graph.addNode(path, { x: Math.cos(angle), y: Math.sin(angle), label: noteLabel(path), size: path === activePath ? 9 : 6, color: path === activePath ? "#8db7ff" : "#71849b" });
    });
    const byLabel = new Map(paths.map((path) => [noteLabel(path).toLocaleLowerCase("th"), path]));
    paths.forEach((path) => extractWikiLinks(notes[path]).forEach((target) => {
      const destination = byLabel.get(noteLabel(target).toLocaleLowerCase("th"));
      if (destination && destination !== path && !graph.hasEdge(path, destination)) graph.addDirectedEdge(path, destination, { color: "#36414f" });
    }));
    const renderer = new Sigma(graph, host.current, { renderEdgeLabels: false, allowInvalidContainer: true });
    return () => renderer.kill();
  }, [notes, activePath]);
  return <div className="mini-graph" ref={host} aria-label="Graph of opened notes" />;
}

function App() {
  const [status, setStatus] = useState<VaultStatus | null>(null);
  const [entries, setEntries] = useState<ExplorerEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [opening, setOpening] = useState(false);
  const [syncBusy, setSyncBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [filter, setFilter] = useState("");
  const [activePath, setActivePath] = useState<string>();
  const [text, setText] = useState("");
  const [documentVersion, setDocumentVersion] = useState(0);
  const [mode, setMode] = useState<"edit" | "read">("edit");
  const [save, setSave] = useState<SaveState>(INITIAL_SAVE);
  const [notes, setNotes] = useState<Record<string, string>>({});
  const [quickOpen, setQuickOpen] = useState(false);
  const [quickQuery, setQuickQuery] = useState("");
  const [quickIndex, setQuickIndex] = useState(0);
  const [leftDrawer, setLeftDrawer] = useState(false);
  const [rightDrawer, setRightDrawer] = useState(false);
  const saveRef = useRef(save);
  const textRef = useRef(text);
  const activePathRef = useRef(activePath);
  const statusRef = useRef(status);
  const noteRequest = useRef(0);
  const explorerRequest = useRef(0);
  const externalNoteRequest = useRef(0);
  const pickerPending = useRef(false);
  const saveInFlight = useRef(false);
  const quickDialog = useRef<HTMLElement>(null);
  const quickInput = useRef<HTMLInputElement>(null);
  const restoreFocus = useRef<HTMLElement | null>(null);
  const restoreDrawerFocus = useRef<HTMLElement | null>(null);
  const explorerDrawerRef = useRef<HTMLElement>(null);
  const contextDrawerRef = useRef<HTMLElement>(null);
  const documentBodyRef = useRef<HTMLDivElement>(null);
  const compactExplorer = useMedia("(max-width: 700px)");
  const compactContext = useMedia("(max-width: 980px)");
  const compactDrawerOpen = (compactExplorer && leftDrawer) || (compactContext && rightDrawer);
  saveRef.current = save;
  textRef.current = text;
  activePathRef.current = activePath;
  statusRef.current = status;

  const loadExplorer = useCallback(async (sessionId: string) => {
    const request = ++explorerRequest.current;
    const all: ExplorerEntry[] = [];
    let after: string | null = null;
    for (let pageIndex = 0; pageIndex < 50; pageIndex += 1) {
      const page: ExplorerPage = await invoke<ExplorerPage>("vault_list_explorer", { sessionId, after, limit: 200 });
      if (page.sessionId !== sessionId) throw new Error("Explorer response identity mismatch");
      all.push(...page.entries);
      if (!page.hasMore) break;
      if (!page.nextAfter) throw new Error("Explorer response is missing its continuation cursor");
      if (page.nextAfter === after) throw new Error("Explorer cursor did not advance");
      after = page.nextAfter;
    }
    if (request !== explorerRequest.current || statusRef.current?.sessionId !== sessionId) return;
    setEntries([...new Map(all.map((entry) => [entry.path, entry])).values()]);
  }, []);

  useEffect(() => {
    invoke<VaultStatus>("vault_status")
      .then(async (next) => {
        statusRef.current = next;
        setStatus(next);
        if (next.active && next.sessionId) await loadExplorer(next.sessionId);
      })
      .catch(() => setError("Native desktop bridge unavailable in browser preview."))
      .finally(() => setLoading(false));
  }, [loadExplorer]);

  useEffect(() => {
    let refreshTimer: number | undefined;
    let disposed = false;
    const refreshActiveNote = async (sessionId: string, path: string) => {
      const request = ++externalNoteRequest.current;
      const note = await invoke<NoteDto>("vault_read_note", { sessionId, path });
      if (disposed || request !== externalNoteRequest.current) return;
      const latestPlan = planVaultChange(
        { sessionId },
        { sessionId: statusRef.current?.sessionId, activePath: activePathRef.current, savePhase: saveRef.current.phase },
      );
      if (latestPlan.type !== "refresh" || latestPlan.activePath !== path) return;
      if (!matchesDocumentIdentity(sessionId, path, note)) throw new Error("Note response identity mismatch");
      if (saveRef.current.revisionHex === note.revisionHex && saveRef.current.byteLen === note.byteLen) return;
      setText(note.text);
      setDocumentVersion((version) => version + 1);
      setNotes((cache) => ({ ...cache, [path]: note.text }));
      setSave(saveReducer(INITIAL_SAVE, { type: "load", revisionHex: note.revisionHex, byteLen: note.byteLen }));
    };
    const unlisten = listen<unknown>("myvault-vault-changed", ({ payload }) => {
      const plan = planVaultChange(
        payload,
        { sessionId: statusRef.current?.sessionId, activePath: activePathRef.current, savePhase: saveRef.current.phase },
      );
      if (plan.type === "ignore") return;
      window.clearTimeout(refreshTimer);
      refreshTimer = window.setTimeout(() => {
        if (disposed) return;
        const latestPlan = planVaultChange(
          { sessionId: plan.sessionId },
          { sessionId: statusRef.current?.sessionId, activePath: activePathRef.current, savePhase: saveRef.current.phase },
        );
        if (latestPlan.type === "ignore") return;
        void loadExplorer(latestPlan.sessionId).catch(() => setError("Could not refresh the Vault after an external change."));
        if (latestPlan.activePath) {
          void refreshActiveNote(latestPlan.sessionId, latestPlan.activePath)
            .catch(() => setError("Could not refresh the active note after an external change."));
        }
      }, VAULT_CHANGE_DEBOUNCE_MS);
    });
    return () => {
      disposed = true;
      window.clearTimeout(refreshTimer);
      void unlisten.then((stop) => stop());
    };
  }, [loadExplorer]);

  const chooseVault = async () => {
    if (pickerPending.current || opening || saveInFlight.current) return;
    if (!canOpenAnotherVault(syncBusy)) {
      setError(SYNC_BUSY_VAULT_MESSAGE);
      return;
    }
    if (statusRef.current?.active && !["clean", "saved"].includes(saveRef.current.phase)) {
      setError("Save or reload the current note before opening another Vault.");
      return;
    }
    pickerPending.current = true;
    setOpening(true);
    setError(undefined);
    try {
      const choice = await invoke<VaultChoice>("vault_choose_folder");
      if (choice.outcome === "activated") {
        const sessionId = choice.status.sessionId;
        if (!choice.status.active || !sessionId) throw new Error("Activated Vault response did not include an active session");
        noteRequest.current += 1;
        explorerRequest.current += 1;
        statusRef.current = choice.status;
        setStatus(choice.status);
        setEntries([]);
        setActivePath(undefined);
        setText("");
        setDocumentVersion((version) => version + 1);
        setNotes({});
        setSave(INITIAL_SAVE);
        setFilter("");
        setQuickQuery("");
        setError(undefined);
        await loadExplorer(sessionId);
      }
    } catch (reason) {
      setError(failureOf(reason).message ?? "Could not open this Vault");
    } finally {
      pickerPending.current = false;
      setOpening(false);
    }
  };

  const openNote = useCallback(async (path: string) => {
    if (!status?.sessionId || path === activePath) return;
    if (!["clean", "saved"].includes(saveRef.current.phase)) {
      setError("Finish or resolve the current note before switching.");
      return;
    }
    setError(undefined);
    const request = ++noteRequest.current;
    const sessionId = status.sessionId;
    try {
      const note = await invoke<NoteDto>("vault_read_note", { sessionId, path });
      if (request !== noteRequest.current || statusRef.current?.sessionId !== sessionId) return;
      if (!matchesDocumentIdentity(sessionId, path, note)) throw new Error("Note response identity mismatch");
      setActivePath(note.path);
      setText(note.text);
      setDocumentVersion((version) => version + 1);
      setNotes((current) => ({ ...current, [note.path]: note.text }));
      setSave(saveReducer(INITIAL_SAVE, { type: "load", revisionHex: note.revisionHex, byteLen: note.byteLen }));
      setLeftDrawer(false);
    } catch (reason) {
      setError(failureOf(reason).message ?? "Could not read this note");
    }
  }, [activePath, status?.sessionId]);

  const saveNow = useCallback(async () => {
    const current = saveRef.current;
    if (!status?.sessionId || !activePath || current.phase !== "dirty" || saveInFlight.current) return;
    const sessionId = status.sessionId;
    const path = activePath;
    const submittedText = textRef.current;
    saveInFlight.current = true;
    setSave((state) => saveReducer(state, { type: "saving" }));
    try {
      const result = await invoke<SaveDto>("vault_save_note", {
        sessionId,
        path,
        text: submittedText,
        expectedRevisionHex: current.revisionHex,
        expectedByteLen: current.byteLen,
      });
      if (!matchesDocumentIdentity(sessionId, path, result)
          || statusRef.current?.sessionId !== sessionId
          || activePathRef.current !== path) {
        setSave((state) => saveReducer(state, { type: "failed", code: "error", message: "Save response identity mismatch" }));
        return;
      }
      const stillCurrent = textRef.current === submittedText;
      setSave((state) => saveReducer(state, { type: "saved", revisionHex: result.revisionHex, byteLen: result.byteLen, stillCurrent }));
      setNotes((cache) => ({ ...cache, [path]: textRef.current }));
    } catch (reason) {
      const failure = failureOf(reason);
      setSave((state) => saveReducer(state, { type: "failed", code: failure.code ?? "error", message: failure.message }));
    } finally {
      saveInFlight.current = false;
    }
  }, [activePath, status?.sessionId]);

  const reloadFromDisk = useCallback(async () => {
    const sessionId = statusRef.current?.sessionId;
    const path = activePathRef.current;
    if (!sessionId || !path) return;
    if (!window.confirm("Reload from disk and discard the current editor buffer?")) return;
    const request = ++noteRequest.current;
    setError(undefined);
    try {
      const note = await invoke<NoteDto>("vault_read_note", { sessionId, path });
      if (request !== noteRequest.current || statusRef.current?.sessionId !== sessionId || activePathRef.current !== path) return;
      if (!matchesDocumentIdentity(sessionId, path, note)) throw new Error("Note response identity mismatch");
      setText(note.text);
      setDocumentVersion((version) => version + 1);
      setNotes((cache) => ({ ...cache, [path]: note.text }));
      setSave(saveReducer(INITIAL_SAVE, { type: "load", revisionHex: note.revisionHex, byteLen: note.byteLen }));
    } catch (reason) {
      setError(failureOf(reason).message ?? "Could not reload this note");
    }
  }, []);

  useEffect(() => {
    if (!shouldAutosave(save)) return;
    const timer = window.setTimeout(() => void saveNow(), 750);
    return () => window.clearTimeout(timer);
  }, [save, saveNow, text]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const command = event.metaKey || event.ctrlKey;
      if (command && event.key.toLowerCase() === "s") {
        event.preventDefault();
        void saveNow();
      }
      if (command && event.key.toLowerCase() === "p") {
        event.preventDefault();
        restoreDrawerFocus.current = null;
        setLeftDrawer(false);
        setRightDrawer(false);
        setQuickOpen(true);
        setQuickQuery("");
        setQuickIndex(0);
      }
      if (event.key === "Escape") {
        setQuickOpen(false);
        setLeftDrawer(false);
        setRightDrawer(false);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [saveNow]);

  useEffect(() => {
    if (quickOpen) {
      restoreFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      window.requestAnimationFrame(() => quickInput.current?.focus());
    } else {
      restoreFocus.current?.focus();
      restoreFocus.current = null;
    }
  }, [quickOpen]);

  useEffect(() => {
    if (mode === "read" && activePath) window.requestAnimationFrame(() => documentBodyRef.current?.focus({ preventScroll: true }));
  }, [activePath, mode]);

  useEffect(() => {
    if (compactDrawerOpen) {
      const drawer = leftDrawer ? explorerDrawerRef.current : contextDrawerRef.current;
      window.requestAnimationFrame(() => drawer?.querySelector<HTMLElement>(".mobile-close")?.focus());
    } else if (restoreDrawerFocus.current) {
      restoreDrawerFocus.current.focus();
      restoreDrawerFocus.current = null;
    }
  }, [compactDrawerOpen, leftDrawer]);

  const openQuickSwitcher = () => {
    restoreDrawerFocus.current = null;
    setLeftDrawer(false);
    setRightDrawer(false);
    setQuickQuery("");
    setQuickIndex(0);
    setQuickOpen(true);
  };

  const openExplorerDrawer = () => {
    if (compactExplorer) restoreDrawerFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setRightDrawer(false);
    setLeftDrawer(true);
  };

  const openContextDrawer = () => {
    if (compactContext) restoreDrawerFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setLeftDrawer(false);
    setRightDrawer(true);
  };

  const markdownEntries = useMemo(() => entries.filter((entry) => entry.kind === "markdown"), [entries]);
  const filtered = useMemo(() => filterEntries(markdownEntries, filter), [markdownEntries, filter]);
  const quickResults = useMemo(() => filterEntries(markdownEntries, quickQuery).slice(0, 12), [markdownEntries, quickQuery]);
  const tree = useMemo(() => buildTree(filtered), [filtered]);
  const outline = useMemo(() => extractOutline(text), [text]);
  const backlinks = useMemo(() => activePath ? Object.entries(notes).filter(([path, body]) => path !== activePath && extractWikiLinks(body).some((link) => noteLabel(link).toLocaleLowerCase("th") === noteLabel(activePath).toLocaleLowerCase("th"))).map(([path]) => path) : [], [activePath, notes]);

  if (loading) return <main className="boot-screen" aria-live="polite"><span className="spinner" />Opening myVault…</main>;
  if (!status?.active || !status.sessionId) {
    return (
      <main className="vault-empty">
        <div className="wordmark"><span>mV</span> myVault</div>
        <section>
          <p className="section-label">LOCAL WORKSPACE</p>
          <h1>Open a folder as your Vault</h1>
          <p>Your notes stay as ordinary Markdown files. After opening a Vault, you can optionally connect Google Drive for read-only metadata browsing.</p>
          <button className="primary-button" disabled={opening} onClick={() => void chooseVault()} type="button">{opening ? "Opening…" : "Choose Vault folder"}</button>
          {error && <p className="inline-error" role="alert">{error}</p>}
        </section>
        <small>Local-first desktop · Optional Drive metadata access</small>
      </main>
    );
  }

  return (
    <main className="workspace">
      <nav className="activity-rail" aria-label="Workspace tools" inert={quickOpen || compactDrawerOpen} aria-hidden={quickOpen || compactDrawerOpen || undefined}>
        <div className="rail-logo">mV</div>
        <button className="active" aria-label="Files" onClick={openExplorerDrawer} type="button">F</button>
        <button aria-label="Quick switcher" onClick={openQuickSwitcher} type="button">Q</button>
        <button aria-label="Context" onClick={openContextDrawer} type="button">C</button>
        <button className="rail-bottom" aria-label="Open another Vault" disabled={!canOpenAnotherVault(syncBusy)} onClick={() => void chooseVault()} type="button">↗</button>
      </nav>

      <aside ref={explorerDrawerRef} className={leftDrawer ? "explorer-panel drawer-open" : "explorer-panel"} aria-label="File explorer" role={compactExplorer && leftDrawer ? "dialog" : undefined} aria-modal={compactExplorer && leftDrawer ? "true" : undefined} inert={quickOpen || (compactExplorer && !leftDrawer) || (compactDrawerOpen && !leftDrawer)} aria-hidden={quickOpen || (compactExplorer && !leftDrawer) || (compactDrawerOpen && !leftDrawer) || undefined} onKeyDown={(event) => { if (compactExplorer && leftDrawer) trapTab(event, explorerDrawerRef.current); }}>
        <header><strong>myVault</strong><button className="mobile-close" onClick={() => setLeftDrawer(false)} aria-label="Close file explorer" type="button">×</button></header>
        <label className="search-box"><span>⌕</span><input value={filter} onChange={(event) => setFilter(event.target.value)} placeholder="Filter notes" aria-label="Filter notes" /></label>
        <div className="panel-title"><span>FILES</span><span>{markdownEntries.length}</span></div>
        <div className="tree-scroll">{tree.length ? <Tree nodes={tree} selected={activePath} onSelect={(path) => void openNote(path)} /> : <p className="panel-empty">No Markdown notes found.</p>}</div>
        <footer><span className="status-led" />Local Vault</footer>
      </aside>

      <section className="document-panel" inert={quickOpen || compactDrawerOpen} aria-hidden={quickOpen || compactDrawerOpen || undefined}>
        <header className="document-toolbar">
          <button className="compact-only" onClick={openExplorerDrawer} aria-label="Open file explorer" type="button">☰</button>
          <div className="document-title"><strong>{activePath ? noteLabel(activePath) : "No note selected"}</strong><span>{activePath ?? "Choose a Markdown file from the explorer"}</span></div>
          <div className="mode-switch" aria-label="Document mode"><button className={mode === "edit" ? "active" : ""} onClick={() => setMode("edit")} type="button">Edit</button><button className={mode === "read" ? "active" : ""} onClick={() => setMode("read")} type="button">Read</button></div>
          <button className="compact-only" onClick={openContextDrawer} aria-label="Open note context" type="button">⋯</button>
        </header>
        {error ? <div className="workspace-alert" role="alert">{error}<button onClick={() => setError(undefined)} aria-label="Dismiss error" type="button">×</button></div>
          : syncBusy && <div className="workspace-notice" role="status">{SYNC_BUSY_VAULT_MESSAGE}</div>}
        {activePath ? (
          <div
            className="document-body"
            ref={documentBodyRef}
            role={mode === "read" ? "region" : undefined}
            aria-label={mode === "read" ? "Markdown reader" : undefined}
            tabIndex={mode === "read" ? 0 : undefined}
            onKeyDown={mode === "read" ? (event) => {
              const command = readerScrollCommand(event.nativeEvent);
              if (!command) return;
              event.preventDefault();
              const scroller = event.currentTarget;
              if (command.type === "edge") scroller.scrollTo({ top: command.edge === "start" ? 0 : scroller.scrollHeight, behavior: "auto" });
              else scroller.scrollBy({ top: command.pages * scroller.clientHeight * 0.9, behavior: "auto" });
            } : undefined}
          >
            {mode === "edit" ? <Editor key={`${activePath}:${documentVersion}`} text={text} onChange={(next) => { setText(next); setSave((state) => saveReducer(state, { type: "edit" })); }} /> : <Reader text={text} />}
          </div>
        ) : (
          <div className="document-empty"><span>M</span><h2>Select a note</h2><p>Use the explorer or press <kbd>⌘/Ctrl</kbd> + <kbd>P</kbd>.</p></div>
        )}
        <footer className="status-bar">
          <span className={`save-state ${save.phase}`}><i />{save.message ?? ({ clean: "Ready", dirty: "Unsaved", saving: "Saving…", saved: "Saved", conflict: "Conflict", unknown: "Verify save", error: "Save failed" }[save.phase])}</span>
          {["conflict", "unknown", "error"].includes(save.phase) && <button className="reload-button" onClick={() => void reloadFromDisk()} type="button">Reload from disk</button>}
          <span>{text ? `${text.trim() ? text.trim().split(/\s+/).length : 0} words · ${new TextEncoder().encode(text).length} bytes` : "UTF-8 Markdown"}</span>
        </footer>
      </section>

      <aside ref={contextDrawerRef} className={rightDrawer ? "context-panel drawer-open" : "context-panel"} aria-label="Note context" role={compactContext && rightDrawer ? "dialog" : undefined} aria-modal={compactContext && rightDrawer ? "true" : undefined} inert={quickOpen || (compactContext && !rightDrawer) || (compactDrawerOpen && !rightDrawer)} aria-hidden={quickOpen || (compactContext && !rightDrawer) || (compactDrawerOpen && !rightDrawer) || undefined} onKeyDown={(event) => { if (compactContext && rightDrawer) trapTab(event, contextDrawerRef.current); }}>
        <header><strong>CONTEXT</strong><button className="mobile-close" onClick={() => setRightDrawer(false)} aria-label="Close context" type="button">×</button></header>
        <SyncPanel key={status.sessionId} sessionId={status.sessionId} onBusyChange={setSyncBusy} />
        <section><h2>Outline</h2>{outline.length ? <ol className="outline-list">{outline.map((item) => <li key={item.id} style={{ paddingLeft: `${(item.level - 1) * 10}px` }}>{item.text}</li>)}</ol> : <p className="panel-empty">Headings appear here.</p>}</section>
        <section><h2>Backlinks <span>{backlinks.length}</span></h2>{backlinks.length ? backlinks.map((path) => <button className="backlink" key={path} onClick={() => void openNote(path)} type="button">{noteLabel(path)}<small>{path}</small></button>) : <p className="panel-empty">Open linked notes to build local context.</p>}</section>
        <section className="graph-section"><h2>Opened-note graph</h2><KnowledgeGraph notes={notes} activePath={activePath} /></section>
      </aside>

      {(leftDrawer || rightDrawer) && <button className="drawer-backdrop" inert={quickOpen} aria-hidden={quickOpen || undefined} onClick={() => { setLeftDrawer(false); setRightDrawer(false); }} aria-label="Close drawer" type="button" />}

      {quickOpen && <div className="dialog-backdrop" role="presentation"><section ref={quickDialog} className="quick-switcher" role="dialog" aria-modal="true" aria-label="Quick switcher" onKeyDown={(event) => trapTab(event, quickDialog.current)}><header><span>⌕</span><input ref={quickInput} value={quickQuery} onChange={(event) => { setQuickQuery(event.target.value); setQuickIndex(0); }} onKeyDown={(event) => { if (event.key === "ArrowDown") { event.preventDefault(); setQuickIndex((index) => Math.min(index + 1, Math.max(0, quickResults.length - 1))); } else if (event.key === "ArrowUp") { event.preventDefault(); setQuickIndex((index) => Math.max(0, index - 1)); } else if (event.key === "Enter" && quickResults[quickIndex]) { setQuickOpen(false); void openNote(quickResults[quickIndex].path); } }} placeholder="Type a note name…" aria-label="Find a note" /></header><div>{quickResults.map((entry, index) => <button className={index === quickIndex ? "active" : ""} key={entry.path} onClick={() => { setQuickOpen(false); void openNote(entry.path); }} type="button"><span>{noteLabel(entry.path)}</span><small>{entry.path}</small>{index === quickIndex && <kbd>Enter</kbd>}</button>)}</div><footer><span>↑↓ navigate</span><span>Esc close</span></footer></section></div>}
    </main>
  );
}

export default App;
