import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { basicSetup } from "codemirror";
import { markdown } from "@codemirror/lang-markdown";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import Graph from "graphology";
import Sigma from "sigma";
import "./App.css";

type PlatformInfo = {
  os: string;
  arch: string;
  family: string;
  debugBuild: boolean;
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
    return () => view.destroy();
  }, []);

  return <div className="editor-host" ref={hostRef} aria-label="Markdown editor probe" />;
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

  useEffect(() => {
    if (!hostRef.current) return;

    const graph = new Graph();
    const count = 60;

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

    return () => renderer.kill();
  }, []);

  return <div className="graph-host" ref={hostRef} aria-label="Knowledge graph probe" />;
}

function App() {
  const [platform, setPlatform] = useState<PlatformInfo | null>(null);
  const [bridgeError, setBridgeError] = useState<string | null>(null);

  useEffect(() => {
    invoke<PlatformInfo>("get_platform_info")
      .then(setPlatform)
      .catch((error: unknown) => setBridgeError(String(error)));
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
            <span className="badge">60 nodes</span>
          </div>
          <GraphProbe />
        </article>
      </section>

      <footer className="spike-footer">
        <span>Next gates</span>
        <strong>Android OAuth · Atomic filesystem · SQLite · Drive fixture</strong>
      </footer>
    </main>
  );
}

export default App;
