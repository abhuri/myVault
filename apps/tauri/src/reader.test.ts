// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { markdownHtml, preventReaderAnchorNavigation } from "./reader";

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
});
