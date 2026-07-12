import DOMPurify from "dompurify";
import { Marked } from "marked";

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[character] ?? character);
}

function highlightCode(code: string, language: string): string {
  const escaped = escapeHtml(code);
  if (!/^(?:js|jsx|ts|tsx|javascript|typescript|json|rust|rs|css|html|bash|sh)$/i.test(language)) return escaped;
  return escaped.replace(
    /\b(const|let|var|function|return|if|else|for|while|async|await|struct|enum|impl|fn|pub|use|match|true|false|null)\b/g,
    '<span class="syntax-keyword">$1</span>',
  );
}

export function markdownHtml(text: string): string {
  type WikiToken = { type: "wikilink"; raw: string; target: string; label: string };
  const marked = new Marked({
    gfm: true,
    breaks: false,
    renderer: {
      html({ text: html }) {
        // Preserve raw anchor text/semantics while removing every author-supplied
        // navigation attribute. Generated Markdown links are handled after sanitize.
        return html.replace(/<a\b[^>]*>/gi, "<a>").replace(/<\/a\s*>/gi, "</a>");
      },
      code({ text: code, lang = "" }) {
        const language = lang.trim().split(/\s+/)[0];
        if (language === "mermaid") {
          return `<pre class="mermaid-source"><code class="language-mermaid">${escapeHtml(code)}</code></pre>`;
        }
        return `<pre data-language="${escapeHtml(language || "text")}"><code class="language-${escapeHtml(language || "text")}">${highlightCode(code, language)}</code></pre>`;
      },
    },
    extensions: [
      {
        name: "wikilink",
        level: "inline",
        start(source) {
          const index = source.indexOf("[[");
          return index >= 0 ? index : undefined;
        },
        tokenizer(source) {
          const match = /^\[\[([^\]|#]+)(?:#[^\]|]+)?(?:\|([^\]]+))?\]\]/.exec(source);
          if (!match) return undefined;
          return {
            type: "wikilink",
            raw: match[0],
            target: match[1].trim(),
            label: (match[2] ?? match[1]).trim(),
          } satisfies WikiToken;
        },
        renderer(token) {
          const wiki = token as WikiToken;
          return `<a class="wiki-link" href="#wiki-${encodeURIComponent(wiki.target)}">${escapeHtml(wiki.label)}</a>`;
        },
      },
    ],
  });
  const raw = marked.parse(text, { async: false }) as string;
  const inertTasks = raw.replace(/<input\b(?=[^>]*\btype="checkbox")[^>]*>/gi, (input) =>
    input.includes("checked")
      ? '<span class="task-checkbox" aria-hidden="true">✓</span>'
      : '<span class="task-checkbox" aria-hidden="true">□</span>',
  );
  const sanitized = DOMPurify.sanitize(inertTasks, {
    ALLOWED_TAGS: ["h1", "h2", "h3", "h4", "h5", "h6", "p", "ul", "ol", "li", "blockquote", "pre", "code", "table", "thead", "tbody", "tr", "th", "td", "a", "strong", "em", "del", "hr", "br", "span"],
    ALLOWED_ATTR: ["href", "title", "class", "data-language", "aria-hidden", "aria-label"],
    FORBID_TAGS: ["style", "form", "input", "button", "textarea", "select", "option", "dialog", "iframe", "object", "embed", "link", "meta", "script"],
    FORBID_ATTR: ["style", "action", "formaction", "srcdoc"],
  });
  const template = document.createElement("template");
  template.innerHTML = sanitized;
  for (const anchor of template.content.querySelectorAll<HTMLAnchorElement>("a")) {
    const href = anchor.getAttribute("href");
    if (!anchor.classList.contains("wiki-link") || !href?.startsWith("#wiki-")) anchor.removeAttribute("href");
  }
  return template.innerHTML;
}

/** Demo reader policy: links remain visible, but no anchor may navigate. */
export function preventReaderAnchorNavigation(event: MouseEvent): void {
  const target = event.target;
  if (target instanceof Element && target.closest("a")) event.preventDefault();
}
