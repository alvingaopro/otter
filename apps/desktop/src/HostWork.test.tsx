import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { HostWork, type Procs } from "./HostWork";
import { HostPage } from "./HostPage";
import { processRows, sessionRows, strays, usageOf } from "./workRows";
import type { HostView, OtterProcess, WorkspaceView } from "./types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

const proc = (extra: Partial<OtterProcess> & { pid: number }): OtterProcess => ({
  started: 1000 + extra.pid,
  name: "x",
  cpu_percent: 1,
  memory: 1024 * 1024,
  busy_secs: 0,
  owner: { kind: "daemon", this: true },
  stoppable: false,
  ...extra,
});

const ws: WorkspaceView = {
  id: "ws_1",
  name: "work",
  activity: "idle",
  state: "ready",
  root: "/tmp/ws",
  brief: {},
  sourceKind: "git",
  source: "feature/x",
  sessions: [{ id: "ses_1", name: "shell", kind: "terminal", status: "running" }],
  attention: [],
  updatedAt: new Date().toISOString(),
} as WorkspaceView;

const host = (extra: Partial<HostView> = {}): HostView =>
  ({ name: "code", describe: "ssh code", status: "connected", agents: [], workspaces: [ws], ...extra }) as HostView;

const owned = { kind: "execution", workspace_id: "ws_1", session_id: "ses_1", execution_id: "ex_1" } as const;
const procs: OtterProcess[] = [
  proc({ pid: 10, name: "otterd" }),
  proc({ pid: 20, parent: 5, name: "zsh", cpu_percent: 2, owner: owned }),
  proc({ pid: 21, parent: 20, name: "vite", cpu_percent: 30, busy_secs: 900, memory: 4 * 1024 * 1024, owner: owned }),
  proc({ pid: 88, name: "otterd", owner: { kind: "daemon", this: false }, stoppable: true, exe: "/old/otterd", home: "/work/.dev/bench/home", busy_secs: 3600 }),
  proc({ pid: 90, name: "node", owner: { kind: "orphan", why: "its workspace is gone" }, stoppable: true }),
];

let root: Root;
let el: HTMLDivElement;
const flush = () => act(async () => new Promise((r) => setTimeout(r, 0)));
const button = (text: string) => [...el.querySelectorAll("button")].find((b) => b.textContent?.trim() === text)!;
const inTable = (text: string) => [...el.querySelectorAll<HTMLElement>(".work-table button")].find((b) => b.textContent?.trim() === text)!;
const text = () => el.textContent ?? "";
const tab = (name: string) => [...el.querySelectorAll<HTMLElement>("[role=tab]")].find((b) => b.textContent?.startsWith(name))!;

describe("what the Work tab is built from", () => {
  it("sums a session's tree and flags what has been busy", () => {
    const [row] = sessionRows([ws], procs);
    expect(usageOf(row.procs).root?.pid).toBe(20);
    expect(row.usage.cpu).toBe(32);
    expect(row.usage.memory).toBe(5 * 1024 * 1024);
    expect(row.flags).toEqual(["busy"]);
  });

  it("flags a running process in an archived workspace as stale", () => {
    const [row] = sessionRows([{ ...ws, state: "archived" }], procs);
    expect(row.flags).toEqual(["busy", "stale"]);
  });

  it("lists strays: another otterd and an orphan, never this otterd or a session's process", () => {
    expect(strays(procs, Date.now()).map((p) => p.pid)).toEqual([88, 90]);
    const rows = processRows(procs, Date.now());
    expect(rows.map((r) => r.proc.pid)).toEqual([88, 90, 10]);
    expect(rows[0].flags).toEqual(["other", "busy"]);
    expect(rows[1].flags).toEqual(["orphan"]);
  });
});

describe("another otterd that has only just started", () => {
  it("is listed, but isn't a stray yet: a task's tests start one per test, for seconds", () => {
    const now = Date.now();
    const young = proc({
      pid: 70,
      name: "otterd",
      owner: { kind: "daemon", this: false },
      stoppable: true,
      started: Math.floor(now / 1000) - 5,
    });
    const old = { ...young, pid: 71, started: Math.floor(now / 1000) - 600 };
    expect(strays([young, old], now).map((p) => p.pid)).toEqual([71]);
    const rows = processRows([young, old], now);
    expect(rows.map((r) => r.proc.pid)).toEqual([70, 71]);
    expect(rows.map((r) => r.flags)).toEqual([[], ["other"]]);
    // An orphan is one at once.
    const orphan = proc({ pid: 72, owner: { kind: "orphan", why: "x" }, stoppable: true, started: Math.floor(now / 1000) - 1 });
    expect(strays([orphan], now)).toHaveLength(1);
  });
});

describe("the Work tab", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    el = document.createElement("div");
    document.body.appendChild(el);
    root = createRoot(el);
    vi.mocked(invoke).mockReset();
    document.hasFocus = () => true;
    try {
      localStorage.clear();
    } catch {
      // Not available.
    }
  });
  afterEach(() => {
    act(() => root.unmount());
    el.remove();
  });

  const work = (p: Procs, extra: Partial<Parameters<typeof HostWork>[0]> = {}) =>
    act(async () => root.render(<HostWork host={host()} tasks={[]} procs={p} now={Date.now()} onTask={vi.fn()} {...extra} />));

  it("lists strays on top and asks again in the row before stopping one, by pid and start time", async () => {
    vi.mocked(invoke).mockResolvedValue(null);
    await work({ state: "ready", procs });
    expect(text()).toContain("2 processes nothing here owns");
    // Which otterd it is: its home, not only where its binary is.
    expect(text()).toContain("home /work/.dev/bench/home");
    // The first Stop in the strip: asked, not sent.
    await act(async () => button("Stop").click());
    expect(invoke).not.toHaveBeenCalled();
    expect(text()).toContain("Asks it to end (SIGTERM).");
    await act(async () => button("Stop").click());
    expect(invoke).toHaveBeenCalledWith("host_process_stop", { host: "code", pid: 88, started: 1088, force: false });
  });

  it("keeps the process when told to", async () => {
    await work({ state: "ready", procs });
    await act(async () => button("Force stop").click());
    await act(async () => button("Keep").click());
    expect(invoke).not.toHaveBeenCalled();
  });

  it("shows a session with its pid, and opens it to its processes", async () => {
    await work({ state: "ready", procs });
    await act(async () => tab("Sessions").click());
    expect(text()).toContain("shell");
    expect(text()).toContain("20");
    expect(text()).toContain("Busy 15m");
    await act(async () => el.querySelector<HTMLElement>("tr.work-row")!.click());
    expect(el.querySelector(".work-tree")!.textContent).toContain("zsh  20");
    expect(el.querySelector(".work-tree")!.textContent).toContain("  vite  21");
  });

  it("runs a session's stop through the session command, not a signal", async () => {
    vi.mocked(invoke).mockResolvedValue(null);
    await work({ state: "ready", procs });
    await act(async () => tab("Sessions").click());
    await act(async () => inTable("Stop").click());
    expect(invoke).not.toHaveBeenCalled();
    await act(async () => inTable("Stop").click());
    expect(invoke).toHaveBeenCalledWith("session_stop", { host: "code", workspace: "ws_1", session: "ses_1" });
  });

  it("says when the host's otterd is too old for the process list, and still lists the rest", async () => {
    await work({ state: "too_old" });
    expect(text()).toContain("too old to list its processes");
    await act(async () => tab("Workspaces").click());
    expect(text()).toContain("work");
  });
});

describe("the host page's tabs", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    el = document.createElement("div");
    document.body.appendChild(el);
    root = createRoot(el);
    document.hasFocus = () => true;
    try {
      localStorage.clear();
    } catch {
      // Not available.
    }
  });
  afterEach(() => {
    act(() => root.unmount());
    el.remove();
  });

  it("opens on Overview, badges Work with the strays, and remembers the tab", async () => {
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "host_processes") return { sampled_at: "", processes: procs };
      if (cmd === "host_metrics") throw "measuring";
      return [];
    });
    const page = () => (
      <HostPage host={host()} version="0.7.46" forwards={[]} tasks={[]} now={Date.now()} onTask={vi.fn()} onRemoved={() => {}} />
    );
    await act(async () => root.render(page()));
    await flush();
    expect(el.querySelector("#host-tab-overview")!.getAttribute("aria-selected")).toBe("true");
    expect(text()).toContain("2 to look at");
    expect(el.querySelector(".host-work")).toBeNull();
    await act(async () => tab("Work").click());
    expect(el.querySelector(".host-work")).not.toBeNull();
    // Another page of the same host starts where this one left off.
    await act(async () => root.render(null));
    await act(async () => root.render(page()));
    await flush();
    expect(el.querySelector(".host-work")).not.toBeNull();
  });
});
