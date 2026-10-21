// Navigation smoke test (D-041): the Activity Bar switches views without
// unmounting the Workspace view, so terminals and their attaches survive.

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HostsPayload } from "./types";

const terminal = vi.hoisted(() => ({ mounts: 0, unmounts: 0 }));
// A host whose otterd is too old to move a session (D-150).
const oldHost = vi.hoisted(() => ({ on: false }));

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
          activity: "needs_you",
          state: "ready",
          root: "/tmp/ws",
          brief: {},
          sourceKind: "empty",
          source: "",
          sessions: [{ id: "ses_1", name: "shell", kind: "terminal", status: "running" }],
          // A finished turn: what looking at the session settles (D-099).
          attention: [
            {
              id: "at_1",
              sessionId: "ses_1",
              kind: "review",
              needsYou: true,
              summary: "shell finished",
              createdAt: new Date().toISOString(),
            },
          ],
          updatedAt: new Date().toISOString(),
        },
        {
          id: "ws_2",
          name: "other",
          activity: "idle",
          state: "ready",
          root: "/tmp/other",
          brief: {},
          sourceKind: "empty",
          source: "",
          sessions: [
            { id: "ses_2", name: "shell", kind: "terminal", status: "running" },
            { id: "ses_3", name: "logs", kind: "terminal", status: "running" },
          ],
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
    if (cmd === "cli_status") return { version: "0.5.0" };
    if (cmd === "update_get") return "0.6.2";
    if (cmd === "session_move" && oldHost.on) throw "invalid request: unknown variant `session.move`";
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
    oldHost.on = false;
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

  it("is an accessible tab list with the Inbox first and selected", () => {
    const tabs = [...host.querySelectorAll('.activity-bar [role="tab"]')];
    // Labels carry a "N need you" count when there is one.
    expect(tabs.map((t) => t.getAttribute("aria-label")?.split(",")[0])).toEqual(["Inbox", "Tasks", "Workspaces"]);
    expect(tab("inbox").getAttribute("aria-selected")).toBe("true");
    expect(tab("inbox").tabIndex).toBe(0);
    expect(tab("tasks").tabIndex).toBe(-1);
    expect(panel("inbox").hidden).toBe(false);
    expect(panel("tasks").hidden).toBe(true);
    expect(panel("workspaces").hidden).toBe(true);
    expect(panel("workspaces").getAttribute("aria-labelledby")).toBe("view-tab-workspaces");
    expect(host.querySelector('[aria-label="Settings"]')).not.toBeNull();
  });

  it("switches views without unmounting the terminal", async () => {
    // The Workspace view is mounted (hidden) from the start.
    expect(terminal.mounts).toBe(1);
    await act(async () => tab("workspaces").click());
    expect(panel("workspaces").hidden).toBe(false);
    expect(panel("tasks").hidden).toBe(true);
    const node = panel("workspaces").querySelector('[data-testid="terminal"]');
    expect(node).not.toBeNull();

    await act(async () => tab("tasks").click());
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
      await act(async () => tab("tasks").click());
    }
    const changing = calls.mock.calls
      .map(([cmd]) => cmd)
      .filter((cmd) => /stop|delete|restart|close|resolve|create|archive|set|input|resize/.test(cmd));
    expect(changing).toEqual([]);
  });

  it("keeps a workspace's terminal attached while another is shown (D-100)", async () => {
    const { invoke } = await import("@tauri-apps/api/core");
    const calls = vi.mocked(invoke);
    const row = (name: string) =>
      [...host.querySelectorAll<HTMLButtonElement>(".ws-row")].find((b) => b.textContent?.includes(name))!;
    await act(async () => tab("workspaces").click());
    expect(terminal.mounts).toBe(1);
    await act(async () => row("other").click());
    await flush();
    expect(terminal.mounts).toBe(2);
    expect(terminal.unmounts).toBe(0);
    expect(host.querySelectorAll(".ws-pane[hidden]").length).toBe(1);

    calls.mockClear();
    await act(async () => row("scratch").click());
    await flush();
    expect(terminal.mounts).toBe(2);
    expect(terminal.unmounts).toBe(0);
    // Shown again without attaching: what attaching settles is settled, by id.
    const resolved = calls.mock.calls.filter(([cmd]) => cmd === "attention_resolve").map(([, args]) => args);
    expect(resolved).toEqual([{ host: "mac", workspace: "ws_1", session: null, attention: "at_1" }]);
  });

  it("shows two sessions side by side, and one again (D-110)", async () => {
    const row = (name: string) =>
      [...host.querySelectorAll<HTMLButtonElement>(".ws-row")].find((b) => b.textContent?.includes(name))!;
    await act(async () => tab("workspaces").click());
    await act(async () => row("other").click());
    await flush();
    const pane = () => host.querySelector<HTMLElement>(".ws-pane:not([hidden])")!;
    const mounts = terminal.mounts;
    const split = () => pane().querySelector<HTMLButtonElement>(".split-btn")!;
    await act(async () => split().click());
    expect(terminal.mounts).toBe(mounts + 1);
    expect(pane().querySelectorAll('[data-testid="terminal"]').length).toBe(2);
    expect(pane().querySelector(".tab.beside")?.textContent).toContain("logs");
    // The tab on the right, clicked: the two trade places.
    const tabNamed = (n: string) => [...pane().querySelectorAll<HTMLButtonElement>(".tab-main")].find((b) => b.textContent?.includes(n))!;
    await act(async () => tabNamed("logs").click());
    expect(pane().querySelector(".tab.selected")?.textContent).toContain("logs");
    expect(pane().querySelector(".tab.beside")?.textContent).toContain("shell");
    const unmounts = terminal.unmounts;
    await act(async () => split().click());
    // The one on the right is hidden, still attached (D-152).
    expect(pane().querySelectorAll(".session-col:not([hidden]) [data-testid=\"terminal\"]").length).toBe(1);
    expect(terminal.unmounts).toBe(unmounts);
  });

  it("keeps a session attached, hidden, while another is shown (D-152)", async () => {
    const row = (name: string) =>
      [...host.querySelectorAll<HTMLButtonElement>(".ws-row")].find((b) => b.textContent?.includes(name))!;
    await act(async () => tab("workspaces").click());
    await act(async () => row("other").click());
    await flush();
    const pane = () => host.querySelector<HTMLElement>(".ws-pane:not([hidden])")!;
    const tabNamed = (n: string) => [...pane().querySelectorAll<HTMLButtonElement>(".tab-main")].find((b) => b.textContent?.includes(n))!;
    const shownTerminals = () => pane().querySelectorAll(".session-col:not([hidden]) [data-testid=\"terminal\"]").length;
    const mounts = terminal.mounts;
    const unmounts = terminal.unmounts;
    await act(async () => tabNamed("logs").click());
    expect(terminal.mounts).toBe(mounts + 1);
    expect(shownTerminals()).toBe(1);
    // Back: shown without attaching again, and the other one is hidden.
    await act(async () => tabNamed("shell").click());
    expect(terminal.mounts).toBe(mounts + 1);
    expect(terminal.unmounts).toBe(unmounts);
    expect(shownTerminals()).toBe(1);
    expect(pane().querySelectorAll(".session-col[hidden]").length).toBe(1);
  });

  it("detaches the sessions beyond the kept ones, as set in Developer (D-152)", async () => {
    const { setKeepSettings } = await import("./keepSessions");
    const row = (name: string) =>
      [...host.querySelectorAll<HTMLButtonElement>(".ws-row")].find((b) => b.textContent?.includes(name))!;
    await act(async () => tab("workspaces").click());
    await act(async () => row("other").click());
    await flush();
    const pane = () => host.querySelector<HTMLElement>(".ws-pane:not([hidden])")!;
    const tabNamed = (n: string) => [...pane().querySelectorAll<HTMLButtonElement>(".tab-main")].find((b) => b.textContent?.includes(n))!;
    try {
      await act(async () => setKeepSettings({ capacity: 1 }));
      await act(async () => tabNamed("logs").click());
      // Only what is shown stays: shell was let go.
      expect(pane().querySelectorAll(".session-col").length).toBe(1);
      expect(terminal.unmounts).toBe(1);
    } finally {
      setKeepSettings({ capacity: 5 });
    }
  });

  it("switches sessions with ⌥1…⌥9, before the terminal sees the keys (D-122)", async () => {
    const row = (name: string) =>
      [...host.querySelectorAll<HTMLButtonElement>(".ws-row")].find((b) => b.textContent?.includes(name))!;
    await act(async () => tab("workspaces").click());
    await act(async () => row("other").click());
    await flush();
    const pane = () => host.querySelector<HTMLElement>(".ws-pane:not([hidden])")!;
    const selected = () => pane().querySelector(".tab.selected")?.textContent;
    // What macOS sends for ⌥2 on a US layout: the key's place, and "™".
    const press = (code: string, init: KeyboardEventInit = {}) => {
      const e = new KeyboardEvent("keydown", { code, key: "™", altKey: true, bubbles: true, cancelable: true, ...init });
      window.dispatchEvent(e);
      return e;
    };
    let e!: KeyboardEvent;
    await act(async () => {
      e = press("Digit2");
    });
    expect(selected()).toContain("logs");
    expect(e.defaultPrevented).toBe(true);
    await act(async () => {
      press("Digit1");
    });
    expect(selected()).toContain("shell");
    // No session in that place: nothing changes.
    await act(async () => {
      press("Digit9");
    });
    expect(selected()).toContain("shell");
    // With Shift too it's not the shortcut: the key goes on to the terminal.
    await act(async () => {
      e = press("Digit2", { shiftKey: true });
    });
    expect(selected()).toContain("shell");
    expect(e.defaultPrevented).toBe(false);
    // Each tab says its key.
    const logsTab = [...pane().querySelectorAll(".tab")].find((t) => t.textContent?.includes("logs"))!;
    expect(logsTab.getAttribute("title")).toContain("⌥2");
  });

  it("puts a session in another place by dragging its tab or from its menu (D-150)", async () => {
    const { invoke } = await import("@tauri-apps/api/core");
    const calls = vi.mocked(invoke);
    const row = (name: string) =>
      [...host.querySelectorAll<HTMLButtonElement>(".ws-row")].find((b) => b.textContent?.includes(name))!;
    await act(async () => tab("workspaces").click());
    await act(async () => row("other").click());
    await flush();
    const pane = () => host.querySelector<HTMLElement>(".ws-pane:not([hidden])")!;
    const names = () => [...pane().querySelectorAll(".tab-name")].map((n) => n.textContent);
    const selected = () => pane().querySelector(".tab.selected")?.textContent;
    const tabNamed = (n: string) => [...pane().querySelectorAll<HTMLButtonElement>(".tab-main")].find((b) => b.textContent?.includes(n))!;
    const moves = () => calls.mock.calls.filter(([cmd]) => cmd === "session_move").map(([, args]) => args);
    // jsdom lays nothing out: tabs 80 wide, side by side, as they come.
    const rect = vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
      const x = [...(this.parentElement?.children ?? [])].indexOf(this) * 82;
      return { left: x, right: x + 80, x, top: 0, bottom: 32, y: 0, width: 80, height: 32, toJSON: () => ({}) } as DOMRect;
    });
    const pointer = (type: string, on: EventTarget, x: number) =>
      on.dispatchEvent(new MouseEvent(type, { bubbles: true, button: 0, clientX: x, clientY: 16 }));
    const drag = async (name: string, from: number, to: number) => {
      await act(async () => {
        pointer("pointerdown", tabNamed(name), from);
        pointer("pointermove", window, to);
      });
    };
    try {
      expect(names()).toEqual(["shell", "logs"]);
      // An otterd too old for it: the tabs go back, and it says to update.
      oldHost.on = true;
      await drag("logs", 120, 20);
      await act(async () => pointer("pointerup", window, 20));
      await flush();
      expect(names()).toEqual(["shell", "logs"]);
      expect(pane().querySelector(".notice.error")?.textContent).toContain("too old for this");
      oldHost.on = false;
      await flush();

      calls.mockClear();
      await drag("logs", 120, 20);
      // Shown in its would-be place while it's dragged.
      expect(names()).toEqual(["logs", "shell"]);
      expect(pane().querySelector(".tab.dragging")?.textContent).toContain("logs");
      await act(async () => {
        pointer("pointerup", window, 20);
        // The click that ends a drag doesn't choose.
        tabNamed("logs").click();
      });
      expect(moves()).toEqual([{ host: "mac", workspace: "ws_2", session: "ses_3", to: 0 }]);
      expect(selected()).toContain("shell");
      expect(pane().querySelector(".tab.dragging")).toBeNull();
      expect(pane().querySelector(".notice.error")).toBeNull();
      // Shown so until the host's order comes back.
      expect(names()).toEqual(["logs", "shell"]);
      expect(pane().querySelector(".tab")?.getAttribute("title")).toContain("⌥1");
      // The keys go by the tabs' order, the dropped one too.
      const option = (code: string) =>
        window.dispatchEvent(new KeyboardEvent("keydown", { code, key: "¡", altKey: true, bubbles: true, cancelable: true }));
      await act(async () => void option("Digit1"));
      expect(selected()).toContain("logs");
      await act(async () => void option("Digit2"));
      expect(selected()).toContain("shell");

      // A press that doesn't go anywhere is a click.
      await flush();
      calls.mockClear();
      await act(async () => {
        pointer("pointerdown", tabNamed("logs"), 40);
        pointer("pointermove", window, 42);
        pointer("pointerup", window, 42);
        tabNamed("logs").click();
      });
      expect(moves()).toEqual([]);
      expect(selected()).toContain("logs");

      // From the menu: the first can only go right.
      const menu = () => pane().querySelector<HTMLButtonElement>('button[aria-label="logs actions"]')!;
      const item = (label: string) => [...document.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')].find((b) => b.textContent === label);
      await act(async () => menu().click());
      expect(item("Move left")).toBeUndefined();
      await act(async () => item("Move right")!.click());
      expect(moves()).toEqual([{ host: "mac", workspace: "ws_2", session: "ses_3", to: 1 }]);
      expect(names()).toEqual(["shell", "logs"]);
    } finally {
      rect.mockRestore();
    }
  });

  it("offers the app's update at the foot of the sidebar, not the command-line tools (D-125)", async () => {
    const { invoke } = await import("@tauri-apps/api/core");
    const calls = vi.mocked(invoke);
    const foot = panel("workspaces").querySelector(".sidebar-foot")!;
    // The tools say another version than the app's; they follow the app now.
    expect(foot.textContent).not.toMatch(/command line tools/i);
    const button = foot.querySelector<HTMLButtonElement>(".update-btn")!;
    expect(button.textContent).toBe("Update to v0.6.2");
    calls.mockClear();
    await act(async () => button.click());
    expect(calls.mock.calls.map(([cmd]) => cmd)).toEqual(["update_install"]);
    expect(button.disabled).toBe(true);
    expect(button.textContent).toBe("Updating…");
  });

  it("checks for an update from the settings menu (D-125)", async () => {
    const { invoke } = await import("@tauri-apps/api/core");
    const calls = vi.mocked(invoke);
    await act(async () => host.querySelector<HTMLButtonElement>('button[aria-label="Settings"]')!.click());
    const item = [...document.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')].find(
      (b) => b.textContent === "Check for updates…",
    )!;
    calls.mockClear();
    await act(async () => item.click());
    expect(calls.mock.calls.map(([cmd]) => cmd)).toEqual(["update_check"]);
  });

  it("shows the performance overlay only once it's turned on (D-128)", async () => {
    const { setPerfOverlay } = await import("./perf");
    expect(document.querySelector(".perf-overlay")).toBeNull();
    await act(async () => setPerfOverlay(true));
    expect(document.querySelector(".perf-overlay")?.textContent).toBe("— fps");
    await act(async () => setPerfOverlay(false));
    expect(document.querySelector(".perf-overlay")).toBeNull();
  });

  it("opens Settings with ⌘,", async () => {
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: ",", metaKey: true, bubbles: true }));
    });
    await flush();
    expect(host.querySelector('[role="dialog"][aria-label="Settings"]')).not.toBeNull();
  });

  it("goes to a workspace with ⌘K", async () => {
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "k", metaKey: true, bubbles: true }));
    });
    const input = host.querySelector<HTMLInputElement>(".quick-input")!;
    expect(input).not.toBeNull();
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      set.call(input, "oth");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect([...host.querySelectorAll(".quick-name")].map((n) => n.textContent)).toEqual(["other"]);
    await act(async () => {
      input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    });
    await flush();
    expect(host.querySelector(".quick")).toBeNull();
    expect(tab("workspaces").getAttribute("aria-selected")).toBe("true");
    expect(host.querySelector(".ws-pane:not([hidden]) h1")?.textContent).toBe("other");
  });

  it("supports the keyboard: arrows in the bar and ⌘1/⌘2/⌘3 anywhere", async () => {
    await act(async () => {
      tab("inbox").dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    });
    expect(tab("tasks").getAttribute("aria-selected")).toBe("true");
    expect(document.activeElement).toBe(tab("tasks"));

    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "3", metaKey: true, bubbles: true }));
    });
    expect(tab("workspaces").getAttribute("aria-selected")).toBe("true");
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "1", metaKey: true, bubbles: true }));
    });
    expect(tab("inbox").getAttribute("aria-selected")).toBe("true");
    expect(terminal.unmounts).toBe(0);
  });
});
