import { describe, expect, it } from "vitest";
import { buildTree, extractOutline, extractWikiLinks, filterEntries, matchesDocumentIdentity, saveReducer, shouldAutosave } from "./workspace";

describe("workspace state", () => {
  it("stops autosave permanently after stale or unknown outcomes", () => {
    const loaded = saveReducer({ phase: "clean", revisionHex: "a", byteLen: 1 }, { type: "edit" });
    expect(shouldAutosave(loaded)).toBe(true);
    const conflict = saveReducer(loaded, { type: "failed", code: "staleRevision" });
    expect(conflict.phase).toBe("conflict");
    expect(shouldAutosave(conflict)).toBe(false);
    expect(saveReducer(conflict, { type: "edit" })).toEqual(conflict);
    const unknown = saveReducer(loaded, { type: "failed", code: "writeOutcomeUnknown" });
    expect(unknown.phase).toBe("unknown");
    expect(shouldAutosave(unknown)).toBe(false);
  });

  it("accepts a successful revised save", () => {
    const state = saveReducer(
      { phase: "saving", revisionHex: "old", byteLen: 1 },
      { type: "saved", revisionHex: "new", byteLen: 4 },
    );
    expect(state).toEqual({ phase: "saved", revisionHex: "new", byteLen: 4 });
  });

  it("keeps newer edits dirty while accepting the submitted revision as the next base", () => {
    const state = saveReducer(
      { phase: "dirty", revisionHex: "old", byteLen: 1 },
      { type: "saved", revisionHex: "submitted", byteLen: 9, stillCurrent: false },
    );
    expect(state).toEqual({ phase: "dirty", revisionHex: "submitted", byteLen: 9 });
    expect(shouldAutosave(state)).toBe(true);
  });

  it("requires response session and path identity", () => {
    expect(matchesDocumentIdentity("session-a", "note.md", { sessionId: "session-a", path: "note.md" })).toBe(true);
    expect(matchesDocumentIdentity("session-a", "note.md", { sessionId: "session-b", path: "note.md" })).toBe(false);
    expect(matchesDocumentIdentity("session-a", "note.md", { sessionId: "session-a", path: "other.md" })).toBe(false);
  });
});

describe("workspace parsers", () => {
  it("infers a stable folder tree and filters Thai paths", () => {
    const entries = [
      { path: "งาน/สอง.md", kind: "markdown" as const, byteLen: 2 },
      { path: "หนึ่ง.md", kind: "markdown" as const, byteLen: 1 },
      { path: "งาน/หนึ่ง.md", kind: "markdown" as const, byteLen: 1 },
    ];
    const tree = buildTree(entries);
    expect(tree[0].type).toBe("folder");
    expect(tree[0].children).toHaveLength(2);
    expect(filterEntries(entries, "สอง").map((entry) => entry.path)).toEqual(["งาน/สอง.md"]);
  });

  it("extracts wiki links and Markdown outline", () => {
    expect(extractWikiLinks("[[Alpha]] [[ไทย#หัวข้อ|ชื่อ]]")).toEqual(["Alpha", "ไทย"]);
    expect(extractOutline("# Title\ntext\n### Detail")).toEqual([
      { level: 1, text: "Title", id: "heading-0" },
      { level: 3, text: "Detail", id: "heading-1" },
    ]);
  });
});
