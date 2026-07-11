import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { basicSetup } from "codemirror";
import { markdown } from "@codemirror/lang-markdown";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import Graph from "graphology";
import Sigma from "sigma";
import { formatMilliseconds, percentile } from "./spike/metrics";
import "./App.css";

type PlatformInfo = {
  os: string;
  arch: string;
  family: string;
  debugBuild: boolean;
};

type GoogleAuthStatus = {
  supported: boolean;
  connected: boolean;
  grantedScopeCount: number;
};

const sampleMarkdown = `# myVault Phase 0

ทดลองพิมพ์ภาษาไทย การเลือกข้อความ และ undo/redo ที่นี่

- [x] Tauri bridge
- [ ] Android IME บนอุปกรณ์จริง
- [ ] Drive round trip

\`\`\`mermaid
flowchart LR
  Local[Local Vault] --> Sync[Sync Engine]
  Sync --> Drive[Google Drive]
\`\`\`
`;

function EditorProbe() {
  const hostRef = useRef<HTMLDivElement>(null);
  const [compositionSamples, setCompositionSamples] = useState<number[]>([]);
  const [documentChanges, setDocumentChanges] = useState(0);

  useEffect(() => {
    if (!hostRef.current) return;

    const state = EditorState.create({
      doc: sampleMarkdown,
      extensions: [
        basicSetup,
        history(),
        keymap.of([...defaultKeymap, ...historyKeymap]),
        markdown(),
        EditorView.lineWrapping,
        EditorView.updateListener.of((update) => {
          if (update.docChanged) setDocumentChanges((count) => count + 1);
        }),
        EditorView.theme({
          "&": { height: "100%", background: "#10151f", color: "#ecf2ff" },
          ".cm-content": { caretColor: "#71e1c4", padding: "16px" },
          ".cm-cursor": { borderLeftColor: "#71e1c4" },
          ".cm-gutters": { background: "#0b1018", color: "#607087", border: "none" },
          ".cm-activeLine, .cm-activeLineGutter": { background: "#172131" },
        }),
      ],
    });

    const view = new EditorView({ state, parent: hostRef.current });
    let compositionStartedAt: number | null = null;
    const onCompositionStart = () => {
      compositionStartedAt = performance.now();
    };
    const onCompositionEnd = () => {
      if (compositionStartedAt === null) return;
      const startedAt = compositionStartedAt;
      compositionStartedAt = null;
      requestAnimationFrame(() => {
        setCompositionSamples((samples) => [...samples.slice(-99), performance.now() - startedAt]);
      });
    };
    view.contentDOM.addEventListener("compositionstart", onCompositionStart);
    view.contentDOM.addEventListener("compositionend", onCompositionEnd);

    return () => {
      view.contentDOM.removeEventListener("compositionstart", onCompositionStart);
      view.contentDOM.removeEventListener("compositionend", onCompositionEnd);
      view.destroy();
    };
  }, []);

  const p95 = percentile(compositionSamples, 0.95);
  return (
    <>
      <div className="editor-host" ref={hostRef} aria-label="Markdown editor probe" />
      <div className="probe-metrics" aria-live="polite">
        <span>Thai composition samples: {compositionSamples.length}</span>
        <span className={p95 !== null && p95 >= 50 ? "metric-fail" : "metric-pass"}>
          p95 composition-to-paint: {formatMilliseconds(p95)}
        </span>
        <span>Document changes: {documentChanges}</span>
      </div>
    </>
  );
}

function MermaidProbe() {
  const [svg, setSvg] = useState("");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    import("mermaid")
      .then(({ default: mermaid }) => {
        mermaid.initialize({
          startOnLoad: false,
          securityLevel: "strict",
          theme: "dark",
        });

        return mermaid.render(
          "phase-zero-flow",
          "flowchart LR; Local[Local Vault] --> Queue[Durable Queue]; Queue --> Drive[Google Drive];",
        );
      })
      .then(({ svg: renderedSvg }) => active && setSvg(renderedSvg))
      .catch((reason: unknown) => active && setError(String(reason)));

    return () => {
      active = false;
    };
  }, []);

  if (error) return <p className="probe-error">Mermaid failed: {error}</p>;

  return <div className="mermaid-host" dangerouslySetInnerHTML={{ __html: svg }} />;
}

function GraphProbe() {
  const hostRef = useRef<HTMLDivElement>(null);
  const [nodeCount, setNodeCount] = useState(1_000);
  const [renderTime, setRenderTime] = useState<number | null>(null);

  useEffect(() => {
    if (!hostRef.current) return;

    const startedAt = performance.now();
    const graph = new Graph();
    const count = nodeCount;

    for (let index = 0; index < count; index += 1) {
      const angle = (Math.PI * 2 * index) / count;
      graph.addNode(`note-${index}`, {
        x: Math.cos(angle),
        y: Math.sin(angle),
        size: index % 10 === 0 ? 8 : 4,
        color: index % 10 === 0 ? "#f2bd5d" : "#71e1c4",
        label: index % 10 === 0 ? `Note ${index}` : undefined,
      });
    }

    for (let index = 0; index < count; index += 1) {
      graph.addEdge(`note-${index}`, `note-${(index + 1) % count}`, {
        color: "#34455c",
      });
      if (index % 3 === 0) {
        graph.addEdge(`note-${index}`, `note-${(index + 10) % count}`, {
          color: "#27354a",
        });
      }
    }

    const renderer = new Sigma(graph, hostRef.current, {
      renderEdgeLabels: false,
      allowInvalidContainer: false,
    });
    const host = hostRef.current;
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        renderer.resize();
        renderer.refresh();
      }
    });
    observer.observe(host);
    let paintFrame = 0;
    const frame = requestAnimationFrame(() => {
      renderer.refresh();
      paintFrame = requestAnimationFrame(() => setRenderTime(performance.now() - startedAt));
    });

    return () => {
      cancelAnimationFrame(frame);
      cancelAnimationFrame(paintFrame);
      observer.disconnect();
      renderer.kill();
    };
  }, [nodeCount]);

  return (
    <>
      <div className="probe-controls" role="group" aria-label="Graph capacity">
        {[1_000, 5_000].map((count) => (
          <button
            className={nodeCount === count ? "active" : ""}
            key={count}
            onClick={() => setNodeCount(count)}
            type="button"
          >
            {count.toLocaleString()} nodes
          </button>
        ))}
        <span>first paint: {formatMilliseconds(renderTime)}</span>
      </div>
      <div className="graph-host" ref={hostRef} aria-label="Knowledge graph probe" />
    </>
  );
}

function GoogleAuthProbe() {
  const [status, setStatus] = useState<GoogleAuthStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    invoke<GoogleAuthStatus>("google_auth_status")
      .then(setStatus)
      .catch((reason: unknown) => setError(String(reason)));
  }, []);

  const run = (command: "google_auth_connect" | "google_auth_disconnect") => {
    setBusy(true);
    setError(null);
    invoke<GoogleAuthStatus>(command)
      .then(setStatus)
      .catch((reason: unknown) => setError(String(reason)))
      .finally(() => setBusy(false));
  };

  return (
    <section className="auth-probe" aria-label="Google authorization probe">
      <div>
        <strong>Google Drive authorization</strong>
        <span>
          {status?.supported
            ? status.connected
              ? `Connected · ${status.grantedScopeCount} scope`
              : "Ready for Android consent"
            : "Android native flow only"}
        </span>
      </div>
      {status?.supported && (
        <button
          disabled={busy}
          onClick={() => run(status.connected ? "google_auth_disconnect" : "google_auth_connect")}
          type="button"
        >
          {busy ? "Waiting…" : status.connected ? "Disconnect" : "Connect Google Drive"}
        </button>
      )}
      {error && <span className="probe-error">{error}</span>}
    </section>
  );
}

function App() {
  const [platform, setPlatform] = useState<PlatformInfo | null>(null);
  const [bridgeError, setBridgeError] = useState<string | null>(null);
  const [visibilityEvents, setVisibilityEvents] = useState(0);

  useEffect(() => {
    invoke<PlatformInfo>("get_platform_info")
      .then(setPlatform)
      .catch((error: unknown) => setBridgeError(String(error)));
  }, []);

  useEffect(() => {
    const recordVisibility = () => setVisibilityEvents((count) => count + 1);
    document.addEventListener("visibilitychange", recordVisibility);
    return () => document.removeEventListener("visibilitychange", recordVisibility);
  }, []);

  return (
    <main className="app-shell">
      <header className="hero">
        <div>
          <p className="eyebrow">PHASE 0 · TECHNICAL SPIKE</p>
          <h1>myVault platform laboratory</h1>
          <p className="hero-copy">
            Validate the native bridge, Thai input, Markdown editor, safe diagrams, and graph rendering
            before the product layer begins.
          </p>
        </div>
        <div className="runtime-card" aria-live="polite">
          <span className="status-dot" />
          {platform ? (
            <>
              <strong>{platform.os}</strong>
              <span>{platform.arch}</span>
              <span>{platform.debugBuild ? "debug build" : "release build"}</span>
            </>
          ) : bridgeError ? (
            <span className="probe-error">Bridge unavailable: {bridgeError}</span>
          ) : (
            <span>Probing native runtime…</span>
          )}
        </div>
      </header>

      <section className="probe-grid">
        <article className="probe-card editor-card">
          <div className="card-heading">
            <div>
              <p className="card-kicker">INPUT + EDITOR</p>
              <h2>Markdown and Thai IME</h2>
            </div>
            <span className="badge">CodeMirror 6</span>
          </div>
          <EditorProbe />
        </article>

        <article className="probe-card">
          <div className="card-heading">
            <div>
              <p className="card-kicker">SAFE RENDERING</p>
              <h2>Diagram pipeline</h2>
            </div>
            <span className="badge">Mermaid strict</span>
          </div>
          <MermaidProbe />
        </article>

        <article className="probe-card">
          <div className="card-heading">
            <div>
              <p className="card-kicker">WEBGL</p>
              <h2>Knowledge graph</h2>
            </div>
            <span className="badge">capacity probe</span>
          </div>
          <GraphProbe />
        </article>
      </section>

      <section className="runtime-evidence" aria-label="Runtime evidence">
        <strong>Runtime evidence</strong>
        <span>{navigator.userAgent}</span>
        <span>Visibility transitions: {visibilityEvents}</span>
      </section>

      <GoogleAuthProbe />

      <footer className="spike-footer">
        <span>Next gates</span>
        <strong>Android OAuth · Atomic filesystem · SQLite · Drive fixture</strong>
      </footer>
    </main>
  );
}

export default App;
