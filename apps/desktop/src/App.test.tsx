// Navigation smoke test (D-041): the Activity Bar switches views without
// unmounting the Workspace view, so terminals and their attaches survive.

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HostsPayload } from "./types";

const terminal = vi.hoisted(() => ({ mounts: 0, unmounts: 0 }));

const payload: HostsPayload = {
  configDir: "/tmp/otter",
  hosts: [
    {
      name: "mac",
      describe: "this Mac",
      status: "connected",
      agents: [],
      workspaces: [
        {
          id: "ws_1",
          name: "scratch",
          activity: "idle",
          state: "ready",
          root: "/tmp/ws",
          brief: {},
          sourceKind: "empty",
          source: "",
          sessions: [{ id: "ses_1", name: "shell", kind: "terminal", status: "running" }],
          attention: [],
          updatedAt: new Date().toISOString(),
        },
      ],
    },
  ],
};

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string) => {
    if (cmd === "hosts_get") return payload;
    if (cmd === "cli_status") return { version: "0.6.1" };
    return [];
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ onFocusChanged: async () => () => {}, setTheme: async () => {} }),
}));
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "0.6.1" }));
vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: async () => true,
  requestPermission: async () => "granted",
  sendNotification: vi.fn(),
}));
vi.mock("./Terminal", async () => {
  const { useEffect } = await import("react");
  return {
    Terminal: () => {
      useEffect(() => {
        terminal.mounts++;
        return () => {
          terminal.unmounts++;
        };
      }, []);
      return <div data-testid="terminal" />;
    },
  };
});

const { default: App } = await import("./App");

let root: Root;
let host: HTMLDivElement;

async function flush() {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 0));
  });
}

const tab = (name: string) => host.querySelector<HTMLButtonElement>(`[role="tab"][aria-controls="view-${name}"]`)!;
const panel = (name: string) => host.querySelector<HTMLElement>(`#view-${name}`)!;

describe("Activity Bar navigation", () => {
  beforeEach(async () => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    localStorage.clear();
    terminal.mounts = 0;
    terminal.unmounts = 0;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    await act(async () => root.render(<App />));
    await flush();
  });

  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  it("is an accessible tab list with Features first and selected", () => {
    const tabs = [...host.querySelectorAll('.activity-bar [role="tab"]')];
    // Labels carry a "N need you" count when there is one.
    expect(tabs.map((t) => t.getAttribute("aria-label")?.split(",")[0])).toEqual(["Features", "Workspaces"]);
    expect(tab("features").getAttribute("aria-selected")).toBe("true");
    expect(tab("features").tabIndex).toBe(0);
    expect(tab("workspaces").tabIndex).toBe(-1);
    expect(panel("features").hidden).toBe(false);
    expect(panel("workspaces").hidden).toBe(true);
    expect(panel("workspaces").getAttribute("aria-labelledby")).toBe("view-tab-workspaces");
    expect(host.querySelector('[aria-label="Settings"]')).not.toBeNull();
  });

  it("switches views without unmounting the terminal", async () => {
    // The Workspace view is mounted (hidden) from the start.
    expect(terminal.mounts).toBe(1);
    await act(async () => tab("workspaces").click());
    expect(panel("workspaces").hidden).toBe(false);
    expect(panel("features").hidden).toBe(true);
    const node = panel("workspaces").querySelector('[data-testid="terminal"]');
    expect(node).not.toBeNull();

    await act(async () => tab("features").click());
    await act(async () => tab("workspaces").click());
    expect(panel("workspaces").querySelector('[data-testid="terminal"]')).toBe(node);
    expect(terminal.mounts).toBe(1);
    expect(terminal.unmounts).toBe(0);
    expect(localStorage.getItem("otter.view")).toBe("workspaces");
  });

  it("never asks a host to change anything when switching views", async () => {
    const { invoke } = await import("@tauri-apps/api/core");
    const calls = vi.mocked(invoke);
    calls.mockClear();
    for (let i = 0; i < 3; i++) {
      await act(async () => tab("workspaces").click());
      await act(async () => tab("features").click());
    }
    const changing = calls.mock.calls
      .map(([cmd]) => cmd)
      .filter((cmd) => /stop|delete|restart|close|resolve|create|archive|set|input|resize/.test(cmd));
    expect(changing).toEqual([]);
  });

  it("supports the keyboard: arrows in the bar and ⌘1/⌘2 anywhere", async () => {
    await act(async () => {
      tab("features").dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    });
    expect(tab("workspaces").getAttribute("aria-selected")).toBe("true");
    expect(document.activeElement).toBe(tab("workspaces"));

    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "1", metaKey: true, bubbles: true }));
    });
    expect(tab("features").getAttribute("aria-selected")).toBe("true");
    expect(terminal.unmounts).toBe(0);
  });
});
