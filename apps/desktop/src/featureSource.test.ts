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
    if (cmd === "features_list") {
      // An otterd from before features answers with an error.
      if (args.host === "old") throw new Error("invalid_request: unknown method feature.list");
      return served.mac;
    }
    return { id: "ft_1", title: "t", status: "draft", updated_at: "2026-10-09T00:00:00Z" };
  }),
}));

const { daemonSource } = await import("./featureSource");
const flush = () => new Promise((r) => setTimeout(r, 0));

describe("daemonSource", () => {
  beforeEach(() => {
    calls.length = 0;
    served.mac = [];
  });

  it("loads connected hosts, tolerates old ones, and follows FeatureChanged", async () => {
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
    handlers.get("host-event")!({ payload: { host: "mac", record: { type: "FeatureChanged" } } });
    await flush();
    expect(s.list()).toHaveLength(2);

    // A host that goes away takes its features with it.
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
        payload: { host: "mac", record: { type: "FeatureStream", feature_id: "ft_a", stream_id: "reply-1", role: "controller", text, done } },
      });
    stream("Hel", false);
    expect(s.drafts!("mac/ft_a")).toEqual([{ stream_id: "reply-1", role: "controller", text: "Hel", done: false }]);
    stream("Hello there.", true);
    expect(s.drafts!("mac/ft_a")[0]).toMatchObject({ text: "Hello there.", done: true });
    // The feature reloads (its message is in it now): the draft goes.
    handlers.get("host-event")!({ payload: { host: "mac", record: { type: "FeatureChanged" } } });
    await flush();
    expect(s.drafts!("mac/ft_a")).toEqual([]);
  });

  it("sends each action with its own command id", async () => {
    const s = daemonSource();
    s.setHosts!(["mac"]);
    await s.act("mac/ft_1", { action: "pause" });
    await s.act("mac/ft_1", { action: "pause" });
    await s.send("mac/ft_1", "hi");
    const ids = calls.filter((c) => c.cmd !== "features_list").map((c) => c.args.commandId as string);
    expect(ids).toHaveLength(3);
    expect(new Set(ids).size).toBe(3);
    expect(ids.every((i) => i.startsWith("cmd_"))).toBe(true);
    const act = calls.find((c) => c.cmd === "feature_act")!;
    expect(act.args).toMatchObject({ host: "mac", feature: "ft_1", action: { action: "pause" } });
  });
});
