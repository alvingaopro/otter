// The workspace view's helpers (D-102 – D-106): links, changes, find, ⌘K,
// the sidebar's order, the files panel's path, text size.

import { describe, expect, it } from "vitest";
import { changesLine, diffLines } from "./changes";
import { findLines } from "./FindPanel";
import { shownPath, typedPath } from "./FilesPanel";
import { logicalLine, urlAtColumn } from "./links";
import { groups, lastActive, matchScore } from "./model";
import { quickMatches } from "./QuickOpen";
import type { HostView, Placed, WorkspaceChanges, WorkspaceView } from "./types";

function ws(name: string, extra: Partial<WorkspaceView> = {}): WorkspaceView {
  return {
    id: `ws_${name}`,
    name,
    activity: "idle",
    state: "ready",
    root: `/w/${name}`,
    brief: {},
    sourceKind: "empty",
    source: "",
    sessions: [],
    attention: [],
    updatedAt: "2026-10-15T08:00:00Z",
    ...extra,
  };
}
const host = { name: "dev", describe: "ssh dev", status: "connected", agents: [], workspaces: [] } as HostView;
const placed = (w: WorkspaceView): Placed => ({ host, ws: w, key: `dev/${w.id}` });

describe("⌘-click links", () => {
  it("finds the link under the column, without trailing punctuation", () => {
    const line = "see https://github.com/o/r/pull/12. and http://localhost:5173/x";
    expect(urlAtColumn(line, 10)).toBe("https://github.com/o/r/pull/12");
    expect(urlAtColumn(line, 34)).toBeUndefined();
    expect(urlAtColumn(line, 50)).toBe("http://localhost:5173/x");
    expect(urlAtColumn(line, 2)).toBeUndefined();
  });

  it("joins rows a long line wrapped into", () => {
    const rows = [
      { text: "go to https://exam", wrapped: false },
      { text: "ple.com/a/b now", wrapped: true },
      { text: "next line", wrapped: false },
    ];
    const { text, offset } = logicalLine(rows, 1);
    expect(offset).toBe(18);
    expect(urlAtColumn(text, offset + 3)).toBe("https://example.com/a/b");
    expect(logicalLine(rows, 2)).toEqual({ text: "next line", offset: 0 });
  });
});

describe("changes", () => {
  const c = (extra: Partial<WorkspaceChanges>): WorkspaceChanges => ({
    git: true,
    ahead: 0,
    behind: 0,
    files: [],
    more: 0,
    insertions: 0,
    deletions: 0,
    ...extra,
  });

  it("say how much changed in one line", () => {
    expect(changesLine(c({}))).toBeUndefined();
    expect(changesLine(c({ git: false }))).toBeUndefined();
    const file = { path: "a", change: "modified" as const, insertions: 3, deletions: 1, uncommitted: true };
    expect(changesLine(c({ files: [file], more: 1, insertions: 12, deletions: 4, ahead: 2 }))).toBe(
      "2 files · +12 −4 · ↑2",
    );
    expect(changesLine(c({ behind: 3 }))).toBe("↓3");
  });

  it("read a diff line by line", () => {
    const kinds = diffLines("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n same\n").map((l) => l.kind);
    expect(kinds).toEqual(["meta", "meta", "meta", "hunk", "del", "add", "same"]);
  });
});

describe("find in a session's history", () => {
  it("marks every match in each line, any case", () => {
    const { found, total } = findLines("ok\nError: one error\nfine\nERROR again", "error");
    expect(total).toBe(2);
    expect(found.map((f) => f.line)).toEqual([2, 4]);
    expect(found[0].parts).toEqual([
      { text: "Error", hit: true },
      { text: ": one ", hit: false },
      { text: "error", hit: true },
    ]);
  });

  it("keeps the latest when there are too many", () => {
    const { found, total } = findLines(Array.from({ length: 10 }, (_, i) => `hit ${i}`).join("\n"), "hit", 3);
    expect(total).toBe(10);
    expect(found.map((f) => f.line)).toEqual([8, 9, 10]);
  });
});

describe("finding a workspace", () => {
  const api = placed(ws("api-login", { sourceKind: "git", source: "otter/login", sessions: [] }));
  const web = placed(ws("web", { brief: { title: "Login page" }, updatedAt: "2026-10-15T09:00:00Z" }));
  const old = placed(ws("api-old", { activity: "archived", updatedAt: "2026-10-15T10:00:00Z" }));

  it("matches every word, the name first", () => {
    expect(matchScore(api, "api")).toBe(4);
    expect(matchScore(api, "login")).toBe(3);
    expect(matchScore(web, "login")).toBe(1);
    expect(matchScore(api, "login api")).toBe(2);
    expect(matchScore(api, "dev api")).toBe(1);
    expect(matchScore(api, "nothing")).toBe(0);
    expect(quickMatches([web, old, api], "api").map((p) => p.ws.name)).toEqual(["api-login", "api-old"]);
    expect(quickMatches([api, web], "").map((p) => p.ws.name)).toEqual(["web", "api-login"]);
  });

  it("lists idle workspaces the latest active first", () => {
    const a = placed(ws("a", { updatedAt: "2026-10-15T08:00:00Z" }));
    const b = placed(
      ws("b", {
        updatedAt: "2026-10-15T07:00:00Z",
        sessions: [
          {
            id: "s",
            name: "claude",
            kind: "agent",
            status: "running",
            agent: { provider: "claude", state: "idle", since: "2026-10-15T09:30:00Z" },
          },
        ],
      }),
    );
    expect(lastActive(b.ws)).toBe("2026-10-15T09:30:00Z");
    expect(groups([a, b])[0].items.map((p) => p.ws.name)).toEqual(["b", "a"]);
  });
});

describe("the files panel's path", () => {
  it("shows the workspace as ~ws and reads it back", () => {
    expect(shownPath("/w/x/src", "/w/x")).toBe("~ws/src");
    expect(shownPath("/w/x", "/w/x")).toBe("~ws");
    expect(shownPath("/etc", "/w/x")).toBe("/etc");
    expect(typedPath("~ws/src/lib ", "/w/x")).toBe("/w/x/src/lib");
    expect(typedPath("~ws", "/w/x")).toBe("/w/x");
    expect(typedPath("docs", "/w/x")).toBe("docs");
  });
});

describe("text size", () => {
  it("steps with ⌘= and ⌘-, back with ⌘0, and is remembered", async () => {
    const { fontSize, zoomKey, DEFAULT_FONT, onFontSize } = await import("./terminalFont");
    const seen: number[] = [];
    const off = onFontSize((n) => seen.push(n));
    const k = (key: string) => zoomKey({ metaKey: true, ctrlKey: false, altKey: false, key });
    expect(k("=")).toBe(true);
    expect(fontSize()).toBe(DEFAULT_FONT + 1);
    expect(k("-")).toBe(true);
    expect(k("-")).toBe(true);
    expect(fontSize()).toBe(DEFAULT_FONT - 1);
    expect(localStorage.getItem("otter.terminal-font-size")).toBe(String(DEFAULT_FONT - 1));
    expect(k("0")).toBe(true);
    expect(fontSize()).toBe(DEFAULT_FONT);
    expect(k("t")).toBe(false);
    expect(zoomKey({ metaKey: false, ctrlKey: false, altKey: false, key: "=" })).toBe(false);
    expect(seen).toEqual([DEFAULT_FONT + 1, DEFAULT_FONT, DEFAULT_FONT - 1, DEFAULT_FONT]);
    off();
  });
});
