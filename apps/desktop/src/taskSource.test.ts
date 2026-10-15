import { beforeEach, describe, expect, it, vi } from "vitest";

type Handler = (e: { payload: unknown }) => void;
const handlers = vi.hoisted(() => new Map<string, Handler>());
const calls = vi.hoisted(() => [] as { cmd: string; args: Record<string, unknown> }[]);
const served = vi.hoisted(() => ({ mac: [] as unknown[] }));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, h: Handler) => {
    handlers.set(name, h);
    return () => {};
  }),
}));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd === "tasks_list") {
      // An otterd from before tasks answers with an error.
      if (args.host === "old") throw new Error("invalid_request: unknown method task.list");
      return served.mac;
    }
    return { id: "ft_1", title: "t", status: "draft", updated_at: "2026-10-09T00:00:00Z" };
  }),
}));

const { daemonSource } = await import("./taskSource");
const flush = () => new Promise((r) => setTimeout(r, 0));

describe("daemonSource", () => {
  beforeEach(() => {
    calls.length = 0;
    served.mac = [];
  });

  it("loads connected hosts, tolerates old ones, and follows TaskChanged", async () => {
    const s = daemonSource();
    const seen: number[] = [];
    s.subscribe((l) => seen.push(l.length));
    served.mac = [{ id: "ft_a", title: "A", status: "draft", updated_at: "2026-10-09T00:00:00Z" }];
    s.setHosts!(["mac", "old"]);
    await flush();
    expect(s.list().map((p) => p.key)).toEqual(["mac/ft_a"]);

    served.mac = [...served.mac, { id: "ft_b", title: "B", status: "draft", updated_at: "2026-10-09T00:00:00Z" }];
    // Some other host's event, or another kind, doesn't reload.
    handlers.get("host-event")!({ payload: { host: "mac", record: { type: "SessionCreated" } } });
    await flush();
    expect(s.list()).toHaveLength(1);
    handlers.get("host-event")!({ payload: { host: "mac", record: { type: "TaskChanged" } } });
    await flush();
    expect(s.list()).toHaveLength(2);

    // A host that goes away takes its tasks with it.
    s.setHosts!([]);
    expect(s.list()).toHaveLength(0);
    expect(seen.length).toBeGreaterThan(0);
  });

  it("shows text being written until its message has loaded", async () => {
    const s = daemonSource();
    s.setHosts!(["mac"]);
    await flush();
    const stream = (text: string, done: boolean) =>
      handlers.get("host-event")!({
        payload: { host: "mac", record: { type: "TaskStream", task_id: "ft_a", stream_id: "reply-1", role: "controller", text, done } },
      });
    stream("Hel", false);
    expect(s.drafts!("mac/ft_a")).toEqual([{ stream_id: "reply-1", role: "controller", text: "Hel", done: false }]);
    stream("Hello there.", true);
    expect(s.drafts!("mac/ft_a")[0]).toMatchObject({ text: "Hello there.", done: true });
    // The task reloads (its message is in it now): the draft goes.
    handlers.get("host-event")!({ payload: { host: "mac", record: { type: "TaskChanged" } } });
    await flush();
    expect(s.drafts!("mac/ft_a")).toEqual([]);
  });

  it("puts the agent's activity on its live run without reloading", async () => {
    served.mac = [
      {
        id: "ft_a",
        title: "A",
        status: "implementing",
        updated_at: "2026-10-09T00:00:00Z",
        runs: [{ id: "run_1", task_id: "t", runtime: "claude", state: "running", started_at: "2026-10-09T00:00:00Z" }],
      },
    ];
    const s = daemonSource();
    s.setHosts!(["mac"]);
    await flush();
    const loads = calls.filter((c) => c.cmd === "tasks_list").length;
    for (const line of ["$ cargo build", "✎ src/a.rs"]) {
      handlers.get("host-event")!({
        payload: { host: "mac", record: { type: "TaskActivity", task_id: "ft_a", run_id: "run_1", line } },
      });
    }
    expect(s.list()[0].task.runs[0].activity).toEqual(["$ cargo build", "✎ src/a.rs"]);
    expect(calls.filter((c) => c.cmd === "tasks_list")).toHaveLength(loads);
  });

  it("sends each action with its own command id", async () => {
    const s = daemonSource();
    s.setHosts!(["mac"]);
    await s.act("mac/ft_1", { action: "pause" });
    await s.act("mac/ft_1", { action: "pause" });
    await s.send("mac/ft_1", "hi");
    const ids = calls.filter((c) => c.cmd !== "tasks_list").map((c) => c.args.commandId as string);
    expect(ids).toHaveLength(3);
    expect(new Set(ids).size).toBe(3);
    expect(ids.every((i) => i.startsWith("cmd_"))).toBe(true);
    const act = calls.find((c) => c.cmd === "task_act")!;
    expect(act.args).toMatchObject({ host: "mac", task: "ft_1", action: { action: "pause" } });
  });
});
