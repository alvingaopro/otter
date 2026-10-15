// A file's diff in the Changes panel (D-111): parsed, folded, paired,
// highlighted.

import { describe, expect, it } from "vitest";
import { CONTEXT, foldDiff, parseDiff, splitRows } from "./diff";
import { highlightLines, languageOf, splitLines } from "./highlight";

/** A unified diff of a 40-line file whose line 20 changed, with all of it as context. */
function whole(): string {
  const body = Array.from({ length: 40 }, (_, i) =>
    i === 19 ? "-line 20\n+line twenty\n+line twenty-one" : ` line ${i + 1}`,
  ).join("\n");
  return `diff --git a/f.txt b/f.txt\nindex 1..2 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,40 +1,41 @@\n${body}\n`;
}

describe("a diff", () => {
  it("reads lines with their numbers on both sides", () => {
    const p = parseDiff(whole());
    expect(p.lines.length).toBe(42);
    const del = p.lines.find((l) => l.kind === "del")!;
    expect([del.text, del.old, del.new]).toEqual(["line 20", 20, undefined]);
    const adds = p.lines.filter((l) => l.kind === "add");
    expect(adds.map((l) => [l.text, l.new])).toEqual([["line twenty", 20], ["line twenty-one", 21]]);
    expect(p.lines[p.lines.length - 1]).toMatchObject({ text: "line 40", old: 40, new: 41 });
    expect(p.oldText.split("\n").length).toBe(40);
    expect(p.newText.split("\n").length).toBe(41);
    expect(p.breaks).toEqual([]);
  });

  it("folds what is far from a change, and opens what is asked for", () => {
    const p = parseDiff(whole());
    const items = foldDiff(p.lines, new Set(), p.breaks);
    expect(items.map((i) => i.kind)).toEqual(["fold", "lines", "fold"]);
    const [top, near, bottom] = items as { from: number; to: number }[];
    expect([top.from, top.to]).toEqual([0, 19 - CONTEXT]);
    expect(near.to - near.from).toBe(CONTEXT * 2 + 3);
    expect(bottom.to).toBe(p.lines.length);
    const opened = foldDiff(p.lines, new Set([0, 1, 2]), p.breaks);
    expect(opened[0]).toMatchObject({ kind: "lines", from: 0, to: 3 });
  });

  it("marks where the diff itself left lines out", () => {
    const p = parseDiff("@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,2 @@\n j\n-k\n+K\n");
    expect(p.breaks).toEqual([3]);
    expect(foldDiff(p.lines, new Set(), p.breaks).map((i) => i.kind)).toEqual(["lines", "skip", "lines"]);
  });

  it("pairs what was removed with what took its place", () => {
    const p = parseDiff(whole());
    const at = p.lines.findIndex((l) => l.kind === "del");
    const rows = splitRows(p.lines, at - 1, at + 4);
    expect(rows.map((r) => [r.left?.text, r.right?.text])).toEqual([
      ["line 19", "line 19"],
      ["line 20", "line twenty"],
      [undefined, "line twenty-one"],
      ["line 21", "line 21"],
    ]);
  });

  it("says a binary file is one", () => {
    expect(parseDiff("diff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n").binary).toBe(true);
  });
});

describe("highlighting", () => {
  it("knows a file's language by its name", () => {
    expect(languageOf("src/main.rs")).toBe("rust");
    expect(languageOf("App.tsx")).toBe("typescript");
    expect(languageOf("Makefile")).toBe("makefile");
    expect(languageOf("notes.txt")).toBeUndefined();
  });

  it("closes a span at each line's end and opens it again on the next", () => {
    expect(splitLines('<span class="a">x\ny</span>z')).toEqual(['<span class="a">x</span>', '<span class="a">y</span>z']);
  });

  it("keeps a comment across lines in its colour, and escapes plain text", () => {
    const lines = highlightLines("/* one\n two */\nlet x = 1;", "rust");
    expect(lines.length).toBe(3);
    expect(lines[1]).toContain("hljs-comment");
    expect(lines[2]).toContain("hljs-keyword");
    expect(highlightLines("a < b", undefined)).toEqual(["a &lt; b"]);
  });
});
