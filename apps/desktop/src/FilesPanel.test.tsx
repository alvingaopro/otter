// Transfer progress (D-103): a panel shows its own transfers, and only those.
// Kept workspaces' panels are mounted too (D-100), and would never clear the
// rows of a transfer another one started.

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { FilesPanel } from "./FilesPanel";

const bus = vi.hoisted(() => ({
  transfer: (_: unknown) => {},
  drop: (_: unknown) => {},
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_: string, f: (e: { payload: unknown }) => void) => {
    bus.transfer = (payload) => f({ payload });
    return () => {};
  }),
}));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: async (f: (e: { payload: unknown }) => void) => {
      bus.drop = (payload) => f({ payload });
      return () => {};
    },
  }),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ ask: vi.fn(), open: vi.fn(), save: vi.fn() }));

let root: Root;
let host: HTMLDivElement;
const flush = () => act(async () => new Promise((r) => setTimeout(r, 0)));
const rows = () => [...host.querySelectorAll(".transfer")].map((r) => r.textContent);

describe("the Files panel's transfers", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
    vi.clearAllMocks();
  });

  it("shows its own upload while it runs, and not another panel's", async () => {
    let finish = () => {};
    vi.mocked(invoke).mockImplementation(async (command: string) => {
      if (command === "fs_list") return { path: "", root: "/r", entries: [] };
      if (command === "file_upload") return new Promise<void>((r) => void (finish = r));
    });
    await act(async () => root.render(<FilesPanel host="mac" workspace="ws_1" onClose={() => {}} />));
    await flush();

    await act(async () => bus.drop({ type: "drop", paths: ["/tmp/a.txt"] }));
    const sent = vi.mocked(invoke).mock.calls.find(([c]) => c === "file_upload")![1] as { id: string };

    await act(async () => bus.transfer({ id: sent.id, name: "a.txt", done: 1, total: 2 }));
    await act(async () => bus.transfer({ id: "up-elsewhere", name: "b.txt", done: 1, total: 2 }));
    expect(rows()).toHaveLength(1);
    expect(rows()[0]).toContain("a.txt");

    await act(async () => finish());
    await flush();
    expect(rows()).toEqual([]);
  });
});
