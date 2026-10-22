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
    if (cmd === "triggers_list") {
      // An otterd from before triggers answers with an error.
      if (args.host === "old") throw new Error("invalid_request: unknown method trigger.list");
      return served.mac;
    }
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
});
