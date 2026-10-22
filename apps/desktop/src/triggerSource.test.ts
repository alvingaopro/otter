import { beforeEach, describe, expect, it, vi } from "vitest";

type Handler = (e: { payload: unknown }) => void;
const handlers = vi.hoisted(() => new Map<string, Handler>());
const calls = vi.hoisted(() => [] as { cmd: string; args: Record<string, unknown> }[]);
const served = vi.hoisted(() => ({ mac: [] as unknown[], failing: false, ran: null as unknown }));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, h: Handler) => {
    handlers.set(name, h);
    return () => {};
  }),
}));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd === "triggers_list") {
      // An otterd from before triggers answers with an error.
      if (args.host === "old") throw new Error("invalid_request: unknown method trigger.list");
      // A hiccup: the connection dropped for a moment.
      if (served.failing) throw new Error("connection reset");
      return served.mac;
    }
    if (cmd === "trigger_run") return served.ran;
    if (cmd === "trigger_update") return { ...(served.mac[0] as object), enabled: args.enabled };
    return null;
  }),
}));

const { daemonTriggers } = await import("./triggerSource");
const flush = () => new Promise((r) => setTimeout(r, 0));
const nightly = { id: "trg_1", name: "nightly", enabled: true, schedule: { kind: "manual" }, policy: {}, history: [] };

describe("daemonTriggers", () => {
  beforeEach(() => {
    calls.length = 0;
    served.mac = [nightly];
    served.failing = false;
  });

  it("loads connected hosts, tolerates old ones, and follows TriggersChanged", async () => {
    const s = daemonTriggers();
    const seen: number[] = [];
    s.subscribe((l) => seen.push(l.length));
    s.setHosts(["mac", "old"]);
    await flush();
    expect(s.list().map((p) => p.key)).toEqual(["mac/trg_1"]);
    calls.length = 0;
    // A task changing isn't a trigger changing.
    handlers.get("host-event")!({ payload: { host: "mac", record: { type: "TaskChanged" } } });
    await flush();
    expect(calls).toEqual([]);
    served.mac = [];
    handlers.get("host-event")!({ payload: { host: "mac", record: { type: "TriggersChanged" } } });
    await flush();
    expect(calls.map((c) => c.cmd)).toEqual(["triggers_list"]);
    expect(s.list()).toEqual([]);
    expect(seen[seen.length - 1]).toBe(0);
  });

  it("puts a paused trigger in place, ahead of the event", async () => {
    const s = daemonTriggers();
    s.setHosts(["mac"]);
    await flush();
    await s.setEnabled("mac/trg_1", false);
    expect(calls[calls.length - 1]).toEqual({ cmd: "trigger_update", args: { host: "mac", trigger: "trg_1", enabled: false } });
    expect(s.list()[0].trigger.enabled).toBe(false);
    await s.remove("mac/trg_1");
    expect(calls[calls.length - 1]).toEqual({ cmd: "trigger_delete", args: { host: "mac", trigger: "trg_1" } });
    expect(s.list()).toEqual([]);
  });

  it("keeps what a host said when it can't answer once, and forgets a host that's gone", async () => {
    const s = daemonTriggers();
    s.setHosts(["mac"]);
    await flush();
    served.failing = true;
    handlers.get("host-event")!({ payload: { host: "mac", record: { type: "TriggersChanged" } } });
    await flush();
    expect(s.list().map((p) => p.key)).toEqual(["mac/trg_1"]);
    served.failing = false;
    // Gone, then back: read afresh, not what it said before.
    s.setHosts([]);
    expect(s.list()).toEqual([]);
    served.mac = [];
    calls.length = 0;
    s.setHosts(["mac"]);
    await flush();
    expect(calls.map((c) => c.cmd)).toEqual(["triggers_list"]);
    expect(s.list()).toEqual([]);
  });

  it("runs with the occurrence it's given, and puts a trigger it didn't have yet in the list", async () => {
    const s = daemonTriggers();
    served.mac = [];
    s.setHosts(["mac"]);
    await flush();
    served.ran = { ...nightly, history: [{ occurrence: "2026-10-22T01:40:00.123Z", at: "2026-10-22T01:40:01Z", task_id: "ft_9" }] };
    const t = await s.run("mac/trg_1", "2026-10-22T01:40:00.123Z");
    expect(calls[calls.length - 1]).toEqual({ cmd: "trigger_run", args: { host: "mac", trigger: "trg_1", occurrence: "2026-10-22T01:40:00.123Z" } });
    expect(t.history[0].task_id).toBe("ft_9");
    expect(s.list().map((p) => p.key)).toEqual(["mac/trg_1"]);
  });
});
