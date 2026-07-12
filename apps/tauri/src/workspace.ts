export type ExplorerEntry = {
  path: string;
  kind: "markdown" | "file";
  byteLen: number;
};

export type TreeNode = {
  name: string;
  path: string;
  type: "folder" | "file";
  entry?: ExplorerEntry;
  children: TreeNode[];
};

export type SavePhase = "clean" | "dirty" | "saving" | "saved" | "conflict" | "unknown" | "error";

export type SaveState = {
  phase: SavePhase;
  revisionHex: string;
  byteLen: number;
  message?: string;
};

export type SaveAction =
  | { type: "load"; revisionHex: string; byteLen: number }
  | { type: "edit" }
  | { type: "saving" }
  | { type: "saved"; revisionHex: string; byteLen: number; stillCurrent?: boolean }
  | { type: "failed"; code: string; message?: string };

export function saveReducer(state: SaveState, action: SaveAction): SaveState {
  switch (action.type) {
    case "load":
      return { phase: "clean", revisionHex: action.revisionHex, byteLen: action.byteLen };
    case "edit":
      if (state.phase === "conflict" || state.phase === "unknown") return state;
      return { ...state, phase: "dirty", message: undefined };
    case "saving":
      return state.phase === "dirty" ? { ...state, phase: "saving" } : state;
    case "saved":
      return {
        phase: action.stillCurrent === false ? "dirty" : "saved",
        revisionHex: action.revisionHex,
        byteLen: action.byteLen,
      };
    case "failed":
      if (action.code === "staleRevision") {
        return { ...state, phase: "conflict", message: "ไฟล์ถูกแก้ไขจากที่อื่น — หยุดบันทึกอัตโนมัติแล้ว" };
      }
      if (action.code === "writeOutcomeUnknown") {
        return { ...state, phase: "unknown", message: "ยังยืนยันผลการบันทึกไม่ได้ — กรุณาเปิดโน้ตใหม่ก่อนเขียนต่อ" };
      }
      return { ...state, phase: "error", message: action.message ?? "บันทึกไม่สำเร็จ" };
  }
}

export function matchesDocumentIdentity(
  expectedSessionId: string,
  expectedPath: string,
  value: { sessionId: string; path: string },
): boolean {
  return value.sessionId === expectedSessionId && value.path === expectedPath;
}

export function shouldAutosave(state: SaveState): boolean {
  return state.phase === "dirty";
}

export function buildTree(entries: ExplorerEntry[]): TreeNode[] {
  const root: TreeNode = { name: "", path: "", type: "folder", children: [] };
  for (const entry of entries) {
    const parts = entry.path.split("/");
    let parent = root;
    parts.forEach((name, index) => {
      const path = parts.slice(0, index + 1).join("/");
      const file = index === parts.length - 1;
      let node = parent.children.find((child) => child.name === name && child.type === (file ? "file" : "folder"));
      if (!node) {
        node = { name, path, type: file ? "file" : "folder", entry: file ? entry : undefined, children: [] };
        parent.children.push(node);
      }
      parent = node;
    });
  }
  const sort = (nodes: TreeNode[]) => {
    nodes.sort((a, b) => (a.type === b.type ? a.name.localeCompare(b.name, ["th", "en"]) : a.type === "folder" ? -1 : 1));
    nodes.forEach((node) => sort(node.children));
  };
  sort(root.children);
  return root.children;
}

export function extractWikiLinks(text: string): string[] {
  return [...text.matchAll(/\[\[([^\]|#]+)(?:#[^\]|]+)?(?:\|[^\]]+)?\]\]/g)]
    .map((match) => match[1].trim())
    .filter(Boolean);
}

export function extractOutline(text: string): Array<{ level: number; text: string; id: string }> {
  return text
    .split(/\r?\n/)
    .map((line) => /^(#{1,6})\s+(.+?)\s*$/.exec(line))
    .filter((match): match is RegExpExecArray => Boolean(match))
    .map((match, index) => ({
      level: match[1].length,
      text: match[2].replace(/\s+#+$/, ""),
      id: `heading-${index}`,
    }));
}

export function filterEntries(entries: ExplorerEntry[], query: string): ExplorerEntry[] {
  const normalized = query.trim().toLocaleLowerCase("th");
  if (!normalized) return entries;
  return entries.filter((entry) => entry.path.toLocaleLowerCase("th").includes(normalized));
}
