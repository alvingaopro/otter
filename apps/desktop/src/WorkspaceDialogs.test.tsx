import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { DeleteWorkspaceDialog, PublishDialog } from "./WorkspaceDialogs";
import type { WorkspaceView } from "./types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

const ws: WorkspaceView = {
  id: "ws_1",
  name: "work",
  activity: "idle",
  state: "ready",
  root: "/tmp/ws",
  brief: {},
  sourceKind: "git",
  source: "feature/x",
  sessions: [],
  attention: [],
  updatedAt: new Date().toISOString(),
} as WorkspaceView;

let root: Root;
let host: HTMLDivElement;
const flush = () => act(async () => new Promise((r) => setTimeout(r, 0)));
const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent?.includes(text))!;
const box = () => document.querySelector<HTMLInputElement>('input[type="checkbox"]')!;

describe("the workspace dialogs", () => {
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

  it("delete never opens with the discard box ticked from before", async () => {
    const open = () => act(async () => root.render(<DeleteWorkspaceDialog host="mac" ws={ws} onClose={() => {}} />));
    await open();
    await act(async () => box().click());
    expect(box().checked).toBe(true);
    await act(async () => root.render(null));
    await open();
    expect(box().checked).toBe(false);
    await act(async () => button("Delete workspace").click());
    expect(invoke).toHaveBeenCalledWith("workspace_delete", { host: "mac", workspace: "ws_1", force: false });
  });

  it("publishes the branch, asks for a pull request and links it", async () => {
    vi.mocked(invoke).mockResolvedValue({
      branch: "feature/x",
      uncommitted: true,
      pullRequest: { number: 7, url: "https://github.com/o/r/pull/7", opened: true },
    });
    await act(async () => root.render(<PublishDialog host="mac" ws={ws} changes={null} onClose={() => {}} />));
    await act(async () => button("Push and open pull request").click());
    await flush();
    expect(invoke).toHaveBeenCalledWith("workspace_publish", {
      host: "mac",
      workspace: "ws_1",
      pullRequest: true,
      title: null,
      body: null,
    });
    expect(document.body.textContent).toContain("pull request #7");
    expect(document.body.textContent).toContain("left out");
  });

  it("says what went wrong, and can be tried again", async () => {
    vi.mocked(invoke).mockRejectedValue("pushing feature/x: rejected");
    await act(async () => root.render(<PublishDialog host="mac" ws={ws} changes={null} onClose={() => {}} />));
    await act(async () => button("Push and open pull request").click());
    await flush();
    expect(document.body.textContent).toContain("rejected");
    expect(button("Push and open pull request").hasAttribute("disabled")).toBe(false);
  });
});
