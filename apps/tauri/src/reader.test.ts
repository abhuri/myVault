// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import {
  markdownHtml,
  planVaultChange,
  preventReaderAnchorNavigation,
  readerScrollCommand,
  renderMermaidSources,
  VAULT_CHANGE_DEBOUNCE_MS,
} from "./reader";

describe("safe Markdown reader", () => {
  it("removes active controls, style injection, scripts, and event surfaces", () => {
    const html = markdownHtml(`
<style>body { display: none }</style>
<form action="https://evil.invalid"><input value="spoof"><button>Save</button></form>
<dialog open>Fake conflict</dialog>
<img src=x onerror="alert(1)">
<p style="position:fixed" onclick="alert(1)">Visible text</p>
<script>alert(1)</script>
`);
    for (const forbidden of ["<style", "<form", "<input", "<button", "<dialog", "<img", "onerror", "onclick", "style=", "action="]) {
      expect(html).not.toContain(forbidden);
    }
    expect(html).toContain("Visible text");
  });

  it("keeps GFM tables, inert tasks, code highlighting, and visible wiki links", () => {
    const html = markdownHtml(`
- [x] Safe task

| Key | Value |
| --- | --- |
| one | two |

\`\`\`ts
const ready = true;
\`\`\`

[[แผนงาน|Project plan]]
`);
    expect(html).toContain("<table>");
    expect(html).toContain('class="task-checkbox"');
    expect(html).not.toContain("<input");
    expect(html).toContain('class="syntax-keyword"');
    expect(html).toContain('href="#wiki-');
    expect(html).toContain("Project plan");
  });

  it("transforms prose wiki links without altering inline code, fences, or Mermaid source", () => {
    const html = markdownHtml(`Prose [[Target|Visible]] and \`[[InlineCode]]\`.

\`\`\`text
[[FencedCode]]
\`\`\`

\`\`\`mermaid
flowchart LR
  A[[Subroutine]] --> B
\`\`\`
`);
    expect(html).toContain('class="wiki-link"');
    expect(html).toContain("Visible");
    expect(html).toContain("[[InlineCode]]");
    expect(html).toContain("[[FencedCode]]");
    expect(html).toContain("A[[Subroutine]] --&gt; B");
    expect(html.match(/class="wiki-link"/g)).toHaveLength(1);
  });

  it("keeps only generated wiki hashes navigable in sanitized markup", () => {
    const html = markdownHtml(`
[HTTPS](https://evil.invalid/path)
[Protocol relative](//evil.invalid/path)
[Email](mailto:steal@example.invalid)
[Relative](../outside.md)
<a href="https://raw.invalid">Raw anchor</a>
<a class="wiki-link" href="#wiki-Spoofed">Spoofed raw wiki</a>
[[Safe Wiki]]
`);
    const template = document.createElement("template");
    template.innerHTML = html;
    const anchors = [...template.content.querySelectorAll("a")];
    expect(anchors.map((anchor) => anchor.textContent?.trim())).toEqual([
      "HTTPS",
      "Protocol relative",
      "Email",
      "Relative",
      "Raw anchor",
      "Spoofed raw wiki",
      "Safe Wiki",
    ]);
    expect(anchors.slice(0, 6).every((anchor) => !anchor.hasAttribute("href"))).toBe(true);
    expect(anchors[6].getAttribute("href")).toBe("#wiki-Safe%20Wiki");
  });

  it("prevents every delegated anchor click, including inert wiki links", () => {
    const reader = document.createElement("article");
    reader.innerHTML = markdownHtml("[[Safe Wiki]]");
    reader.addEventListener("click", preventReaderAnchorNavigation);
    const anchor = reader.querySelector("a");
    expect(anchor).not.toBeNull();
    const click = new MouseEvent("click", { bubbles: true, cancelable: true });
    const dispatched = anchor!.dispatchEvent(click);
    expect(dispatched).toBe(false);
    expect(click.defaultPrevented).toBe(true);
  });

  it("normalizes Mermaid fence names case-insensitively", () => {
    const html = markdownHtml("```Mermaid\nflowchart LR\n  A --> B\n```");
    expect(html).toContain('class="mermaid-source"');
  });

  it("isolates Mermaid failures and continues rendering later diagrams", async () => {
    const host = document.createElement("div");
    host.innerHTML = markdownHtml("```mermaid\nbroken\n```\n\n```mermaid\nflowchart LR\n A --> B\n```");
    const nodes = [...host.querySelectorAll<HTMLElement>("pre.mermaid-source")];
    await renderMermaidSources(nodes, async (_id, source) => {
      if (source.includes("broken")) throw new Error("invalid diagram");
      return { svg: '<svg><script>alert(1)</script><text>safe</text></svg>' };
    });
    expect(host.querySelector("pre.render-error")?.getAttribute("aria-label")).toBe("Mermaid diagram could not be rendered");
    expect(host.querySelector("figure.mermaid-diagram")?.getAttribute("role")).toBe("img");
    expect(host.innerHTML).toContain("safe");
    expect(host.innerHTML).not.toContain("<script");
  });

  it("stops Mermaid DOM writes after the reader is disposed", async () => {
    const host = document.createElement("div");
    host.innerHTML = markdownHtml("```mermaid\nflowchart LR\n A --> B\n```");
    const node = host.querySelector<HTMLElement>("pre.mermaid-source")!;
    await renderMermaidSources([node], async () => ({ svg: "<svg />" }), () => false);
    expect(host.querySelector("pre.mermaid-source")).toBe(node);
    expect(host.querySelector("figure")).toBeNull();
  });

  it("debounces only the current opaque session and protects unsafe active buffers", () => {
    const current = { sessionId: "opaque-a", activePath: "note.md", savePhase: "clean" };
    expect(VAULT_CHANGE_DEBOUNCE_MS).toBe(150);
    expect(planVaultChange(null, current)).toEqual({ type: "ignore" });
    expect(planVaultChange({ sessionId: "opaque-b" }, current)).toEqual({ type: "ignore" });
    expect(planVaultChange({ sessionId: "opaque-a" }, current)).toEqual({
      type: "refresh",
      sessionId: "opaque-a",
      activePath: "note.md",
    });
    for (const savePhase of ["dirty", "saving", "conflict", "unknown", "error"]) {
      expect(planVaultChange({ sessionId: "opaque-a" }, { ...current, savePhase })).toEqual({
        type: "refresh",
        sessionId: "opaque-a",
        activePath: undefined,
      });
    }
  });

  it("maps reader keyboard navigation without intercepting unrelated shortcuts", () => {
    const key = (value: string, overrides: Partial<KeyboardEvent> = {}) => readerScrollCommand({
      key: value,
      metaKey: false,
      ctrlKey: false,
      altKey: false,
      shiftKey: false,
      ...overrides,
    });
    expect(key("PageDown")).toEqual({ type: "page", pages: 1 });
    expect(key(" ", { shiftKey: true })).toEqual({ type: "page", pages: -1 });
    expect(key("ArrowDown", { metaKey: true })).toEqual({ type: "edge", edge: "end" });
    expect(key("ArrowUp", { ctrlKey: true })).toEqual({ type: "edge", edge: "start" });
    expect(key("s", { metaKey: true })).toBeUndefined();
    expect(key("PageDown", { altKey: true })).toBeUndefined();
  });
});
