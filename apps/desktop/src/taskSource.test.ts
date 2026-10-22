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

  it("drops a draft begun and ended with no words, and what was streaming when the host comes back (D-195)", async () => {
    const s = daemonSource();
    s.setHosts!(["mac"]);
    await flush();
    const stream = (stream_id: string, text: string, done: boolean) =>
      handlers.get("host-event")!({
        payload: { host: "mac", record: { type: "TaskStream", task_id: "ft_a", stream_id, role: "controller", text, done } },
      });
    stream("watch-w1", "", false);
    expect(s.drafts!("mac/ft_a")).toEqual([{ stream_id: "watch-w1", role: "controller", text: "", done: false }]);
    stream("watch-w1", "", true);
    expect(s.drafts!("mac/ft_a")).toEqual([]);
    // Begun before the connection dropped: it may never end.
    stream("reply-2", "", false);
    handlers.get("host-resync")!({ payload: { host: "mac" } });
    expect(s.drafts!("mac/ft_a")).toEqual([]);
    // Its end missed, but Otter's message is here: the draft goes with the load.
    stream("reply-msg_3", "", false);
    stream("watch-w9:1", "", false);
    served.mac = [
      {
        id: "ft_a",
        title: "A",
        status: "review",
        updated_at: "2026-10-09T00:00:00Z",
        messages: [{ id: "msg_4", role: "controller", text: "Here.", at: "2026-10-09T00:00:00Z", correlation_id: "msg_3" }],
      },
    ];
    handlers.get("host-event")!({ payload: { host: "mac", record: { type: "TaskChanged" } } });
    await flush();
    expect(s.drafts!("mac/ft_a").map((d) => d.stream_id)).toEqual(["watch-w9:1"]);
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

  it("keeps each activity line's time and when the agent was last heard from", async () => {
    const run = { id: "run_1", task_id: "t", runtime: "claude", state: "running", started_at: "2026-10-09T00:00:00Z" };
    served.mac = [
      { id: "ft_a", title: "A", status: "implementing", updated_at: "2026-10-09T00:00:00Z", runs: [{ ...run, heard_at: "2026-10-09T00:00:01Z" }] },
    ];
    const s = daemonSource();
    s.setHosts!(["mac"]);
    await flush();
    const activity = (line: string, at?: string) =>
      handlers.get("host-event")!({ payload: { host: "mac", record: { type: "TaskActivity", task_id: "ft_a", run_id: "run_1", line, at } } });
    activity("$ cargo build", "2026-10-09T00:00:05Z");
    const r = () => s.list()[0].task.runs[0];
    expect(r().activity_at).toEqual(["2026-10-09T00:00:05Z"]);
    expect(r().heard_at).toBe("2026-10-09T00:00:05Z");
    // A line without its time (an older otterd): no times rather than wrong ones.
    activity("✎ src/a.rs");
    expect(r().activity).toHaveLength(2);
    expect(r().activity_at).toBeUndefined();
  });

  it("reads a host with a live run again now and then", async () => {
    vi.useFakeTimers();
    try {
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
      await vi.advanceTimersByTimeAsync(0);
      const loads = () => calls.filter((c) => c.cmd === "tasks_list").length;
      const before = loads();
      await vi.advanceTimersByTimeAsync(30_000);
      expect(loads()).toBe(before + 1);
      // Nothing live: nothing to read again.
      served.mac = [{ id: "ft_a", title: "A", status: "review", updated_at: "2026-10-09T00:00:00Z", runs: [] }];
      await vi.advanceTimersByTimeAsync(30_000);
      await vi.advanceTimersByTimeAsync(30_000);
      expect(loads()).toBe(before + 2);
      s.setHosts!([]);
    } finally {
      vi.useRealTimers();
    }
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
